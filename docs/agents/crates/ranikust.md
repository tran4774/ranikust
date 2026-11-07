# ranikust

> **Status:** in progress · **Phase:** 1-2 ·
> **Depends on:** `rnk-metrics` (subscriber, report), `rnk-rootfs` (start-up
> probe), `rnk-sched`, `rnk-sandbox` and `rnk-snapshot` (probes), `rnk-store`
> (`prune`), the build crates ·
> **Used by:** nothing · **Path:** `crates/ranikust/`

**TL;DR:** The binary: subcommands, kaniko-compatible flags, the process
set-up that must run while single-threaded, and the orchestration of a build
across the libraries (`build.rs`). Start no thread (tokio, rayon, a log
writer) before `unshare`, the probes and the state directory's checks.

## Owns

- Parsing the subcommands and flags: `build` (the default), `prune` (only
  `--all` and `--state-dir`; by size and age in Phase 5, ADR-0032) and
  `version` are built; `push` (an OCI layout only, ADR-0052) is later;
  there is no `login` (ADR-0049).
  Kaniko's common flags plus `--mode` (ADR-0011, ADR-0023).
- Process set-up while single-threaded: a private mount namespace when
  permitted, the state directory's checks (`statedir`, ADR-0050), the probes
  of `rnk-sandbox` and `rnk-snapshot` in order, the outcome; with
  `--mode=auto` as root, the user-namespace probe, logged and reported only
  (`report_user_namespace`; its directory left in place when a child of the
  probe was not reaped); in kaniko mode
  the root (`/` in a container, `flock`ed; else a directory per stage,
  `KanikoStages`, ADR-0059). Then the hand-off to the libraries. `prune`
  only calls `rnk-store`.
- The build's user namespace with the store opened in it
  (`setup::enter_user_namespace`, #75): the state directory's checks
  outside, the user's subordinate ids (`subids.rs`: the first range of
  `/etc/subuid` and `/etc/subgid` by name, from `/etc/passwd`, or by uid;
  a missing file gives none, one that cannot be read refuses),
  `rnk_sandbox::enter_build_user_namespace`, then `Store::open`, whose
  clean-up removes a dead build's files of subordinate ids. A refusal
  once the process may be in the namespace is `Kind::Broken`, which never
  routes to another mode: the process must end. No build calls it yet:
  rootless overlay mode (#79) will; a user other than root is still
  refused before the state directory (`NEEDS_ROOT`).
- `prune --all` (`prune.rs`, ADR-0105, #76): root refused first (euid 0
  in any user namespace), naming Phase 5's `prune`; then
  `statedir::check` (a build's checks, outside, with nothing made and no
  mode changed): a missing or empty state directory left so; then
  `setup::enter_build_userns` (the namespace, no store opened), with its
  own guidance where the user gets no namespace
  (`prune::no_user_namespace`; the build's sends the user to root); in it,
  `RLIMIT_NOFILE` raised and `rnk_store::Store::prune_all`. The commands a
  guidance gives name the state directory, shell-quoted, under
  `flock -xn` on it, so they fail while a build holds it; a prune that did
  not finish (`NotPruned`, and a build's `PruneCutShort`) gets the ways
  out (`prune::unfinished`). Prints one line on stdout; a refusal exits 1,
  `prune` without `--all` 2.
- `BuildConfig`, the validated settings of a build (`src/config.rs`, until a
  design assigns it to a library).

## Must not

Boundaries: [`main.rs`](../../../crates/ranikust/src/main.rs) `# Boundaries`.
In short:

- Hold the rule of a single concern: Dockerfile semantics, scheduling,
  executing an instruction, snapshots, the sandbox, filesystem access,
  storage, layers, images and registries live in the library crates. It
  orchestrates a build across them (`build.rs`, ADR-0067).
- Keep global state (paths as `rnk-store`'s `BuildPaths`, settings as
  values), probe a capability itself (each crate probes its own; the kernel
  check is `rnk_rootfs::probe_support`), re-implement stub cleanup, or fill
  or empty a directory the operator names (ADR-0050).

## API

Modules of `crates/ranikust/src/` (all crate-private):

| Module | What it holds |
|---|---|
| `main.rs` | `main` (no `#[tokio::main]`: the runtime is built by hand after set-up), `#[global_allocator]` mimalloc (ADR-0020), the metrics wiring, `check_metrics_file`/`write_metrics_file`, `kaniko_snapshotter`, signal handling |
| `cli.rs` | `Command { Build(Box<BuildArgs>), Prune(PruneArgs), Version }` (no subcommand = `Build`), `parse`, `REFUSED` |
| `prune.rs` | `run` (`prune`'s flags, the default state directory), `prune_all` (root refused), `empty` (`statedir::check`, the namespace, then `Store::prune_all`), `Pruned`, `summary`; the guidance (`no_user_namespace`, `unfinished`, shell-quoted paths) |
| `config.rs` | `BuildConfig::from_args` (kaniko's checks, no I/O), `meta_options`, `overlay_mode_flags`, `Mode { Auto, Overlay, Kaniko }`, `read_seccomp` (the `--seccomp` file, read and compiled into a `Seccomp`) |
| `setup.rs` | `enter_namespace`, `enter_user_namespace` (`UsernsSetup`, no caller yet) and `enter_build_userns` (no state-directory step, no store, for `prune`), `set_up` (overlay), `set_up_kaniko`, `Outcome`, `Refusal`, `Kind`, `Route`/`route`, `host_without_mounts`, `store_refusal`, `probe_overlay`, `report_user_namespace` |
| `statedir.rs` | `prepare` (ADR-0050 checks; its advice names the effective user's default), `check` (the same, making and changing nothing, for `prune`), `SYSTEM_DIRS`, `on_overlayfs`, `raise_nofile_limit` |
| `subids.rs` | `lookup`: the user's first subordinate uid and gid ranges, for the build's user namespace |
| `container.rs` | `detect`: kaniko's `GetContainerRuntime`, ported (ADR-0040, ADR-0048) |
| `exposure.rs` | warnings for secret-looking `--build-arg` values and `DOCKER_AUTH_CONFIG`, `proc_sys_read_only`, `scan_peers` (a process of another mount namespace in reach, ADR-0101) |
| `limits.rs` | `--limit <name>=<value>`: bounds on what a registry or image can make ranikust hold or wait for (ADR-0079, T15) |
| `startup.rs` | the kernel check (ADR-0028) |
| `build.rs` | `run`: the Dockerfile, the stage graph, the context walk, `rnk-sched` over a `StageRunner`, outputs |

- `cli::parse(args)` reports refused kaniko flags first (none ignored,
  ADR-0049; old spellings name the flag to use, ADR-0052), then clap. A
  known kaniko flag that is not accepted is a row in `REFUSED`
  (`Refusal::Renamed`, `NotYet`, `OutOfScope`, `Unneeded` or `Refused`) and in
  [kaniko-map.md#flags](../kaniko-map.md#flags) (ADR-0036); it is refused by
  name with the reason, never left to clap's "unexpected argument".
- Flags worth knowing, with their refusals: the context is a plain path
  (`dir://` refused, exit 2, ADR-0053, ADR-0054; `--context-sub-path`
  applies to fetched contexts only, a local one with it is refused);
  `--ignore-path` globs (`/` and bad patterns refused early). By kind of
  refusal: `Refused` for good by a decision, `--parallel-stages` (ADR-0060)
  and `--single-snapshot` (ADR-0065); `Unneeded` (ranikust changes nothing),
  `--force` (ADR-0050), `--ignore-var-run`, `--whitelist-var-run` (ADR-0052
  D4); `NotYet`, until Phase 4: `--materialize` (ADR-0064), `--cache*`;
  until Phase 5: git contexts, registry mirrors and maps, credential
  helpers, `--registry-client-cert`; not scheduled: `--dryrun`; `OutOfScope`
  (ADR-0008): cloud contexts and registry configuration, `--cache-dir` (the
  warmer); `Renamed`: kaniko's old spellings (ADR-0052) and `--kaniko-dir`,
  kaniko's state directory, now `--state-dir` (ADR-0050). `--workdir` is not
  refused: it is an unknown flag, a clap error with exit 2 (ADR-0050).
  Planned: `--userns` (Phase 3). Ours: `--mode`, `--state-dir`,
  `--jobs`, `--network`, `--secret`, `--allow`, `--seccomp`, `--limit`,
  `--metrics-file`, `--snapshot-workers`.
- `--seccomp=default|unconfined|<file>` (ADR-0103, ADR-0109): read by
  `config::read_seccomp` right after `from_args`, so before any set-up or
  refusal on `/`: the path is pinned with `O_PATH` and must be a regular
  file (`fstat`) before it is reopened for reading through
  `/proc/thread-self/fd`, so a FIFO or device is never opened; at most
  1 MiB, it is compiled by `rnk_sandbox::SeccompProfile::from_json`; any
  failure is exit 2.
  `set_up` and `set_up_kaniko` hand it to the sandbox (`Seccomp::apply`); no
  flag keeps the sandbox's own default, Docker's profile. Kaniko mode on `/`
  refuses every value in `refuse_unsafe_slash`, exit 2.
- `--snapshot-mode` is kaniko's (`full|redo|time`) and keeps that meaning in
  kaniko mode; the mode is `--mode`. Overlay mode warns and ignores it
  (ADR-0023). In overlay mode `overlay_mode_flags` refuses kaniko flags that
  change the image and warns for the rest (ADR-0049).
- `Outcome` has one variant, `OverlayRoot { sandbox: Caps, overlay:
  OverlayCaps }`; kaniko mode is set up by `set_up_kaniko`
  (`KanikoSetup`). Planned: an overlay-rootless outcome (Phase 3, #41); each
  library receives only its own part [open: #3].
- `Refusal { kind, reason, guidance }`: what failed, what to do (ranikust's
  own text, which may name the state directory; `main` escapes both as
  `rnk_metrics::terminal_text` does). `Refusal::needs_kaniko_mode()` lets
  auto pick kaniko mode.
- `statedir::prepare(state_dir)`: the nearest existing directory and every
  real parent walked by descriptor, owned by root or the user and not
  writable by group or others (root's sticky directories excepted); created
  `0700` by `mkdirat` below the checked descriptor; chain checked again.
  `statedir::check(state_dir)`: the same rules on an existing directory,
  `Found { Missing, Empty, Entries }`, nothing made or chmodded. Details: [design/kaniko-mode.md#host-safety-checks](../../design/kaniko-mode.md#host-safety-checks).

### Set-up order

Each step before the next; design in
[modes-and-detection](../../design/modes-and-detection.md#probe-order---modeauto).

0. First thing `main` does, in every mode and before any credential is read
   (ADR-0051): the process is made non-dumpable (`PR_SET_DUMPABLE 0`) and its
   soft `RLIMIT_CORE` is set to 0 (`rnk_sandbox::forbid_core_dumps`; every
   `RUN` gets the original back).
1. Parse the flags (no I/O, so `--help` works on any kernel).
2. The Linux 5.8 check (`rnk_rootfs::probe_support`, ADR-0028), skipped by
   `version`. Then, for `build` only, `rnk_sandbox::run_from_sealed_copy`
   (ADR-0101) runs the process again from a sealed copy of the binary,
   which starts over at step 0, asking for the walk of the writable mounts
   that may reach the binary only when `may_be_kaniko_on_slash` holds (as
   root, without `--mode=overlay`, and `container::detect` finds a
   container: kaniko mode on `/` alone reads it); the copy's run logs the
   way at debug after step 3, or a warning when no way worked (the build
   goes on).
   `RANIKUST_DEBUG_FAULT=seal-tmpfile` skips the memfd, `seal-none` both
   ways.
3. `--metrics-file` refused in the build context or through a symlink
   (`check_metrics_file`, ADR-0076); `rnk_metrics::init` (no thread);
   `--metrics-file` over `RANIKUST_METRICS_FILE` (ADR-0037).
4. `BuildConfig::from_args` and `config::read_seccomp` (in the copy: the
   `--seccomp` file is opened only after step 2's re-exec), then the mode
   (ADR-0040): `--mode=kaniko` goes to kaniko mode; `--mode=overlay`
   checks the kaniko-mode flags and its state directory, then enters the
   namespace; `--mode=auto` enters the
   namespace first and goes to kaniko mode where it is refused to root for
   lack of privilege (`needs_kaniko_mode`), or where overlayfs cannot use the
   state directory outside a container (`setup::route` on the refusal's
   `Kind`: a failed sandbox step or a probe mount that never reached the
   kernel never routes there; per-stage directories, `can_mount`).
5. **Overlay mode** (`setup::set_up`): `enter_namespace`
   (`rnk_sandbox::enter_build_namespace`: refuses in a plain `chroot`, then
   `unshare(CLONE_NEWNS)`, `/` rslave) → `statedir::prepare`, refused on
   overlayfs (a container's root: mount a volume) → `rnk_store::Store::open`
   (marker, start-up clean-up) → with `--mode=auto`,
   `report_user_namespace` (before the overlay probe, whose joined `mount(2)`
   thread may still be counted) → `rnk_sandbox::probe` in
   `<state-dir>/sandbox` → `rnk_snapshot::probe` in
   `builds/<id>/overlay-probe` → outcome → `Sandbox::start` (journal replay,
   spawner thread). `RANIKUST_DEBUG_FAULT=overlay-refused` takes a passed
   overlay probe as a failed probe check, which routes as NFS's refused
   mount does.
6. **Kaniko mode** (`kaniko` in `main.rs`, then `setup::set_up_kaniko`,
   ADR-0050): `NofileLimit::current()` is read first (for `RUN`); root is
   required; the root is picked by `container::detect`. On `/`,
   `refuse_unsafe_slash` runs in `main.rs`, before anything is written: `/`
   is refused where root may mount (ADR-0054), with `--seccomp` (ADR-0103,
   exit 2), with `--network=none` (ADR-0057),
   with a writable `/proc/sys` (`exposure::proc_sys_read_only`, ADR-0054) and
   with ranikust's binary on a writable mount other than `/`'s, reached by one
   (its tree bound read-write too, another hard link; in a root that is not an
   overlay (overlayfs or fuse-overlayfs), a binary in the root too), or one
   whose mount cannot be told (`refuse_writable_binary` on `Sealing::binary()`,
   ADR-0101: qemu-user, a memfd, a mount table that does not place it or changed
   during the walk, a writable directory whose name was removed, a writable
   mount that is covered or cannot be reached at its mount point, a binary not
   weighed because no container was found at the start (`NotWeighed`), or a
   failed call, each with its own guidance; a link of it bound read-write, even
   a removed one, is a reach), and with a process of its PID namespace that has
   a thread in another mount namespace whose root it opens
   (`refuse_reachable_peers` on `exposure::scan_peers`, ADR-0101: once as it
   starts, each thread of `/proc/<pid>/task` through one descriptor of
   `/proc/<pid>`, the process as one where its threads may not be listed;
   ranikust's own namespaces are the one `build` opens before
   `setup::enter_namespace`, held open until the scan returns, and the one it is
   in; it counts only what it opens then (`EACCES`/`EPERM`: not opened), gone
   skipped (an `ns/mnt` denied once its `ns` is gone too), any other error (with
   its step) or more than `PEER_SCAN_LIMIT` threads refused, each with its own
   guidance); then the warnings for secret-looking build arguments and file
   secrets.
   `RLIMIT_NOFILE` is raised (`statedir::raise_nofile_limit`); on `/`, with
   `--jobs` a warning that stages run one at a time and `--jobs` only
   bounds the uploads, and `rnk_sandbox::block_stop_signals`. Then
   `set_up_kaniko`: off `/`, a host where root may not mount is refused
   (`host_without_mounts`). On `/`, `trim_to_run_capabilities` first,
   while single-threaded, and it stays before `Store::open_anonymous`,
   which deletes names a `RUN` can reach
   (ADR-0078); then the `flock` on `/` itself (one build per container), the
   state directory checks, `Store::open_anonymous` (a state directory holding
   a persistent store or held by another build is refused with the store's
   own message, `store_refusal`, ADR-0077) and `Sandbox::start_direct`
   (ADR-0051); after the trim, `exposure::warn_credential_holders`. Off `/`:
   the state directory, `Store::open`, the sandbox probe and
   `Sandbox::start` (`RUN` in the full sandbox, `--jobs` honoured). On `/`
   and off, `--mode=auto` runs `report_user_namespace` just before the
   sandbox starts; off `/`, not when overlay mode's set-up ran it before it
   was refused (`KanikoBy::AutoProbed`: one probe line per build); when
   `set_up_kaniko` returns, `main.rs` builds `KanikoStages`
   (`stages_snapshotter`). Credentials
   go only over verified TLS (`verified_credentials_only`, `build.rs`).
   Then the ignore list (`kaniko_snapshotter`: mountinfo, the flags, the
   state directory when inside the root, physical paths; a state directory
   that is a directory of `/` reached through another mount is refused by
   mountinfo's device and `fs_root`, `same_mount_only_inside`); the context,
   outputs and file secrets refused in the root unless ignored
   (`paths_outside_the_root`).
7. `build::run`: overlay mode or `Snapshots::Kaniko`. Kaniko's squashed units
   run one at a time on the shared root (`kaniko_units`: per stage what
   later units read, kept with `SourceRoot::keep` in
   `<state>/builds/<id>/kept` before the root is reused); the root is emptied
   before the next base and at the end.
   `COPY --from=<bare word>` naming no stage and a `COPY`/`ADD` source
   expanding to an empty string are refused in both modes (ADR-0052 D2).

### Build orchestration (`build.rs`)

- A tokio runtime starts after the single-threaded set-up. `rnk-sched` runs
  a `StageRunner` that pulls bases and `COPY --from` images up front (on a
  blocking thread: the registry client's futures are not `Send` for every
  lifetime) and builds each stage on a thread of its own, which asks the
  scheduler for a `--jobs` permit per `RUN` over a channel. Then the outputs
  in kaniko's order (OCI layout, Docker tar, push) and the digest on stdout.
- Kaniko mode off `/` (ADR-0061, ADR-0062): `stage_readers` counts each
  stage's readers from the stage graph; a stage built `FROM` a stage with
  exactly one reader gets `take_base` (decided from the graph, never from
  scheduling). Each stage counts its `FROM`, `COPY --from` and `RUN --mount
  from=` reads off when it ends; a root nobody reads any more goes at once to
  one release thread (`Releaser`); `release_all` hands it the rest and waits
  before the store closes. Roots are released children first, then the store
  closed, on success and failure alike; a failed release leaves the stages
  below it to the next build's clean-up.
- `--secret` takes buildctl's and kaniko's syntax (`id=`, `type=file|env`,
  `src=`, `env=`; without a source the variable named by the id, else the
  file), ids unique; a file secret must exist and fit 500 KiB at the start.
  `--allow network.host` is BuildKit's entitlement (ADR-0032).
- The roots of `COPY --from=<image>` are made by the stage's thread, not the
  preparation, which only pulls. A Dockerfile not found as given is read
  inside the context through its `RootPath`, never by a host path.
- The first SIGINT or SIGTERM, or a failed stage, cancels the build and kills
  the running `RUN`s (`Sandbox::cancel`, as BuildKit); a second signal exits
  at once. In kaniko mode on `/`, `main` blocks both before any thread starts
  (`rnk_sandbox::block_stop_signals`) and a thread of their own obeys only
  the kernel's and those from outside the PID namespace or ranikust's
  parent: a `RUN`'s are logged and ignored, and the store's marker is
  written back before the second one's exit (ADR-0077). There an anonymous
  store whose close fails, or that is still in use at the end, fails the
  build (`build::close_store`).

### Exit codes

0 success (`--help`, `--version`, `version` included); 1 a runtime error, an
unsupported kernel or a refused environment; 2 a usage error, a refused
kaniko flag or an invalid combination.

## Invariants

- Single-threaded until the spawner thread: in a multi-threaded process
  `unshare(CLONE_NEWNS)` moves only the caller; `CLONE_NEWUSER` fails
  `EINVAL`.
- Release builds abort on panic: nothing reachable from flags or the
  environment may `unwrap`, `expect` or index; no cleanup in `Drop`.
- One static-pie musl binary: no interpreter, no `NEEDED`, no OpenSSL.
- An accepted kaniko flag means what it means in kaniko, or is rejected with a
  message; never accepted and silently different (ADR-0011).
- kaniko's variables are not read (ADR-0049); only `DOCKER_AUTH_CONFIG`
  (`build.rs`, registry credentials) stays. Defaults `--context=/workspace/`,
  `--dockerfile=Dockerfile`; `--kaniko-dir` is `--state-dir` (default
  `/var/lib/ranikust` as root, else `~/.local/share/ranikust` from an
  absolute `$HOME`, or a usage error asking for `--state-dir`; ADR-0050).

## Refs

- **Design:** [design/modes-and-detection.md](../../design/modes-and-detection.md) ([probe order](../../design/modes-and-detection.md#probe-order---modeauto), [build root and state directory](../../design/modes-and-detection.md#build-root-and-state-directory), [refusal and guidance](../../design/modes-and-detection.md#refusal-and-guidance)); [design/sandbox.md#process-set-up-once-per-build](../../design/sandbox.md#process-set-up-once-per-build); [design/kaniko-mode.md#host-safety-checks](../../design/kaniko-mode.md#host-safety-checks)
- **Decisions:** [ADR-0050](../../adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md) (the build root, the state directory), [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (flags, subcommands), [ADR-0023](../../adr/0023-mode-flag.md) (`--mode`, `--snapshot-mode`), [ADR-0022](../../adr/0022-common-state-dir.md) (`--state-dir` defaults), [ADR-0028](../../adr/0028-minimum-kernel-linux-5-8.md) (start-up check), [ADR-0021](../../adr/0021-rnk-store-crate.md) (`prune`), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (static musl, mimalloc, rustls), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (refuse with guidance), [ADR-0013](../../adr/0013-jobs-default-available-parallelism.md), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0010](../../adr/0010-run-network-host-or-none.md), [ADR-0098](../../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md) (planned, Phase 3: a build in a user namespace, one ranikust creates or one it is started in, refuses an image with an owner the namespace does not map, reading the map from `/proc/self/uid_map` and `gid_map` in the second case), [ADR-0100](../../adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) (planned, Phase 3: where the probe is tested), [ADR-0103](../../adr/0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md) and [ADR-0109](../../adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md) (the default seccomp profile, `--seccomp`; [#82](https://github.com/tran4774/ranikust/issues/82)), [ADR-0104](../../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md) (planned, Phase 3: `--userns`, root's range read from a `ranikust` entry of `/etc/subuid` and `/etc/subgid`; [#83](https://github.com/tran4774/ranikust/issues/83)), [ADR-0105](../../adr/0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md) (`prune --all`, refused when root runs it), [ADR-0107](../../adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md) (planned, Phase 3: routing in a user namespace ranikust did not create)
- **Open:** [#3](https://github.com/tran4774/ranikust/issues/3) (the type that carries the probe outcome), [#4](https://github.com/tran4774/ranikust/issues/4) (where the probe mounts its test overlay); tasks [#38](https://github.com/tran4774/ranikust/issues/38) (`unit_chain`); Phase 3 [#41](https://github.com/tran4774/ranikust/issues/41)
- **Evidence:** [exp-05 Q1](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) (5.1 MB static-pie runs in an empty rootfs), [Q2](../../research/experiments/05-rust-ecosystem.md#q2-allocator) (musl 2.8 Mops/s vs 663-753 with mimalloc, 16 threads)
- **kaniko:** `kaniko@44aa7ddc0 cmd/executor/cmd/root.go:336-389` (build flags), `:391-410` (registry flags), `:71-119` (env, mirrors), `:177-188` and `:200-202` (output checks), `:240-245` (container check), `:595-633` (context); `cmd/executor/cmd/push.go:40-80`; `login.go:46-60`
- **Flag status:** [kaniko-map.md#flags](../kaniko-map.md#flags), column "Now".
- **External:** <https://docs.rs/clap/latest/clap/>, <https://crates.io/crates/mimalloc>

## Tests

Tiers and commands: [`docs/testing.md`](../../testing.md#tiers).

- **T0, unit** (`src/*/tests.rs`): `cli` (kaniko spellings per accepted flag,
  pflag booleans, `--target` lists, every refused flag with its reason all
  at once, nothing after `--`, clap errors; `--parallel-stages` and
  `--single-snapshot` refused whatever the spelling), `config` (kaniko's
  checks, contexts, build arguments, absolute paths, state-dir defaults
  as root and below `$HOME`, a `$HOME` that is missing, relative or holds
  `..` refused,
  `meta_options` leaking no destination, path or argument, the overlay-mode
  refusals and warnings, whole-root `--ignore-path` refused, `--seccomp`
  values and files refused for each reason, a FIFO given as the file never
  opened, under an inotify watch), `setup` (the
  outcome from the probes' results, which refusals route `--mode=auto` to
  kaniko mode, a probe failure other than a refused mount never routes, nor
  a refusal once the process may be in the build's user namespace, no
  guidance sends a user back to the mode they used, the user-namespace
  probe's line, which never reads as a mode line, the probe's directory kept when a child of the steps or of
  the overlay probe was left, and only then, a store refusal naming its
  state directory once; `setup/tests/userns.rs`: the
  store opened inside the build's user namespace, as root of it, or the
  set-up refused before the store, whatever the machine allows; `prune
  --all` emptying a state directory there, or refused before it with the
  directory unchanged, no process or mount left), `prune` (`--all` and
  `--state-dir` parsed, `--kaniko-dir` refused; without `--all` a usage
  error naming Phase 5; root refused before anything, a missing directory
  left missing, an empty one left as it is, one without the marker
  refused before the namespace, modes unchanged; the guidance's commands:
  paths quoted so bash reads them back byte for byte, the `flock` failing
  while a store is open, nothing made at a missing path; the ways out of
  a prune that did not finish, in `prune`'s refusal and a build's),
  `statedir`
  (each ADR-0050 refusal: `/`, system directories, symlinks, writable
  parents, nothing created through a planted link; `0700`; a user's default
  below `$HOME` passes, a group-writable home is refused with that user's
  advice, a `$HOME` reached through a `/proc` link is refused with nothing
  made; a regular `.ranikust-pruning` counts as the marker, for the store
  to refuse), `subids` (ranges by name or uid, the first well-formed one,
  malformed and empty ones and lines of `/etc/subuid` and `/etc/subgid`
  not UTF-8 passed over, a `/etc/passwd` comment field not UTF-8 hiding
  no name, an end past
  32 bits refused, both files needed, none for root, an unreadable file an
  error; its red team: files of many lines with binary, overlong and NUL
  lines and the user's malformed ranges first give that user's first
  well-formed range only),
  `container` (every signal
  of kaniko's container check on a fake host, in kaniko's order),
  `exposure` (`scan_peers`' decisions on a fake probe: the first process
  reached, what cannot be told, the bound counted in threads; `walk`'s
  on fake threads: a later thread reached, threads passed over or gone, a
  process whose threads may not be listed; the real probe on each of
  ranikust's own threads and a missing pid; one thread, or the process
  as one, on its `/proc/self`, either of its two namespaces its own, and
  on a fake directory: the step a failure names, a thread gone or out of
  reach; a denied `ns/mnt` gone only with its `ns`; the handle on a
  namespace, an open `O_PATH`, close-on-exec descriptor of its nsfs file;
  a zombie passed over as gone, its roots opening for no one; escaped
  names, a process named by its pid alone; the credential warning's text).
  `src/main.rs` tests: the order of the refusals on `/`, `--seccomp`'s
  among them; `kaniko_mode_on_slash_refuses_a_binary_on_a_writable_mount`,
  `kaniko_mode_on_slash_refuses_a_reachable_process_of_another_mount_namespace`
  and `only_a_build_that_may_be_kaniko_on_slash_walks_the_mounts`
  (ADR-0101); `--snapshot-mode` and `--snapshot-workers` reach the
  walk, `--ignore-path` reaches the ignore lists of `/` and of a stage root
  and never uncovers protected paths. `build.rs`: `kaniko_units` (what each
  unit keeps; every stage another unit reads is a unit of its own; hostile
  `--from` values planned or refused, never a panic, kaniko-oss#724) and the
  bare `COPY --from` refusal.
- **T0, the binary** (`tests/cli.rs`, `tests/metrics.rs`): exit codes of
  `--help`, `version`, refusals and validation; the kernel refusal, the
  seccomp message and the emulator message (Rosetta) through
  `RANIKUST_DEBUG_FAULT=kernel-missing|kernel-blocked|kernel-hidden`;
  `a_build_runs_from_a_sealed_copy_made_once` (a memfd, once, no way
  passed over, the binary's mount logged; `seal-tmpfile`, `seal-none` and
  markers set from outside, which warn);
  `red_team_a_hostile_tmpdir_only_loses_the_tmpfile_copy` (a file, a FIFO
  or a missing directory as `TMPDIR` costs only the `O_TMPFILE` copy);
  `--metrics-file` over the variable; the report on every exit path, panic
  included (`RANIKUST_DEBUG_FAULT=panic|error`); a refused flag exits 2 before
  any work (no state directory, the value never echoed, no `RANIKUST_*`
  variable read); a `--seccomp` file that cannot be read or compiled, or an
  empty value, exits 2 in every mode before any work. `kaniko_mode_needs_root`,
  `without_root_the_set_up_refuses_before_touching_the_state_dir` and
  `without_root_the_set_up_never_makes_the_default_state_dir` run only
  as a normal user (skipped as root): nothing is created, a `--state-dir`
  is left empty with `ranikust.setup` in the report, and without one
  `$HOME` is left empty in every `--mode`. `prune`: without `--all` exit 2
  naming Phase 5; `--all` as root exit 1 naming Phase 5, as a user a
  missing state directory, the default one included, left missing (exit
  0); as a user, a directory without the marker refused unchanged, and a
  state directory emptied or, without a user namespace, refused with
  `prune`'s guidance, naming it under `flock`, and unchanged.
  `red_team_a_regular_user_s_dead_build_links_out_are_removed_and_never_followed`
  (`setup/tests/userns.rs`): the T1 red team's links out as the user who
  ran sudo, with files of subordinate ids, through `newuidmap`'s map;
  where AppArmor restricts user namespaces (24.04) the set-up is refused
  and nothing is made.
- **T1** (`just smoke`, `RANIKUST_CASE_MODE=kaniko just smoke-case`): the
  release binary's set-up and builds in rootless podman (overlay as root in
  a user namespace; kaniko mode on the container's `/`, `RUN` `Direct`);
  the checks of `smoke_checks` in `tests/smoke/lib.bash`; refused inside a
  plain `chroot`. Cases: `tests/smoke/cases/`; `kaniko-flags` tests the
  meaning of `--snapshot-mode` and `--ignore-path` (in overlay mode
  `--ignore-path` is refused and `--snapshot-mode` changes nothing) and, in
  the kaniko leg (a default container), `--mode=auto` picking kaniko mode
  and its user-namespace probe passing with an rbind of `/proc` (#74);
  `userns-probe` the probe's red team (one line per build, nothing of it
  seen by a `RUN`, `RANIKUST_DEBUG_FAULT=overlay-refused` in a container,
  a state directory on tmpfs, ranikust killed mid-probe);
  `seccomp` the default profile, unconfined and a file, and in the kaniko leg
  every `--seccomp` refused and a `RUN` under the container's profile;
  `binary-mounts` is the red team of `refuse_writable_binary` (ADR-0101),
  a directory root and names removed on the host after the binds included;
  `binary-mounts-made` its mount tables made inside the container (covered
  and stacked binds, a mount added during the walk, a container told only
  once ranikust started); `pid-peers` the test of `refuse_reachable_peers`
  ([its steps](../../testing.md#quick-smoke-cases)). `just test-rootless`
  (profile `default`):
  `the_store_in_the_user_namespace_removes_a_dead_build_of_other_ids`
  (`setup/tests/userns.rs`: root of a rootless Podman container; a build
  dies with files and a `0555` directory of ids 1 and 2 in its `tmp/` and
  `builds/` directories; the next store, opened in the namespace, removes
  them); `red_team_a_dead_build_s_links_out_are_removed_and_never_followed`
  (the same build also leaves symlinks out, absolute, climbing and at the
  end of a deep tree, hardlinks to files outside the state directory of
  root and of ids 1 and 2, a loop, a FIFO, a socket and a `0000`
  directory, and `tmp/` and `builds/` entries and a lock file that are
  symlinks out: the links go, what they lead to keeps its contents,
  owners and modes); `red_team_a_home_of_another_user_gives_no_state_directory`
  (as uid 3000: a `$HOME` of another uid, a sticky one of another uid or
  of its own, a group-writable one and `/` give a default state directory
  refused with nothing made; its own home gets one, `0700`);
  `prune_all_empties_a_state_directory_of_other_ids` (a build in the
  namespace leaves files and `0555` directories of ids 1 and 2 in a layer,
  a cache mount and its own directories, with the links out above:
  `prune --all`'s emptying, below the root check, leaves the directory
  empty for `rmdir`, nothing outside changed, no process or mount left);
  `prune_all_leaves_what_a_running_build_uses` (refused, nothing removed,
  while another store is open; emptied once it closed);
  `prune_all_refuses_root_of_a_rootless_container`.
- **T2** (VM; `just test-vm smoke quick`, `RANIKUST_QUICK_MODE=kaniko` for
  kaniko mode; `tests/vm/guest/quick.sh`): set-up as root, on `--privileged`
  Docker (`pivot_root`) and `SYS_ADMIN` under Docker's seccomp (`MS_MOVE`);
  all quick cases in both modes; `rt_*` checks (`container=docker` on the VM
  never builds on `/`, `--mode=auto` with `--ignore-path` refused after
  overlay mode's set-up leaves nothing, a ramfs state directory refused or
  built with a directory per stage, root without `CAP_SYS_ADMIN` refused
  before the state directory exists); `auto_routes_to_stages` and
  `auto_overlay_refused_after_probe` (`--mode=auto` routes, one probe line
  each: [testing](../../testing.md#quick-smoke-cases));
  `slash_lock_refuses_a_second_build`, whose first build on `/` must run
  from a sealed memfd (ADR-0101);
  `seccomp_on_slash` (every `--seccomp` refused, exit 2, in kaniko mode on
  a default Docker container's `/`, and a `RUN` there under the container's
  own profile);
  `adr0054_refusals` (kaniko mode on `/` refused with `--privileged` and
  with `systempaths=unconfined`, before anything is pulled);
  `binary_mounts_on_slash` (ADR-0101: the binary bound read-write, file or
  directory, refused with the `:ro` guidance before the state directory
  exists, and so are the binary bound `:ro` from a tree bound read-write too
  and a file with another hard link; bound `:ro`, its tree `:ro` too, or
  copied into the container's root, it builds; the
  only test of `refuse_unsafe_slash` calling `refuse_writable_binary`);
  `rt_auto_privileged_container` (`--mode=auto` in a `--privileged`
  container is refused, with or without `/.dockerenv`). The smoke suite
  prints the user-namespace probe's line of each environment's build, which
  must pass on the VM itself and stop at `unshare` in a default Docker
  container (#74). A `--state-dir` at
  `/`, below `/home/<user>` or in a directory others may write
  ([testing](../../testing.md#planned-test-suites)). `unit`: `exposure.rs`
  `a_namespace_left_but_held_keeps_its_number_from_new_ones` (ADR-0101: a
  mount namespace no process is in, held by the handle, keeps its number
  from 64 new ones in each of three rounds; the control, without the
  handle, gets it back in one round at least);
  `setup/tests/userns.rs`
  `a_regular_user_s_store_removes_a_dead_build_of_subordinate_ids` (as the
  user who ran sudo: where AppArmor restricts user namespaces, Ubuntu
  24.04, refused before the store with the guidance; elsewhere, 22.04, a
  dead build's files of subordinate ids, which the user cannot remove
  outside, removed by the next store in the namespace);
  `a_regular_user_s_prune_all_empties_a_state_directory_of_subordinate_ids`
  (the same user's whole `prune --all`: on 24.04 refused with `prune`'s
  guidance, the directory unchanged; on 22.04, after a build through
  `newuidmap` with links out, the state directory emptied, then removed
  by the user with `rmdir`, nothing outside changed, no process or mount
  left).
- **T2c:** `setup/tests.rs` `a_second_build_on_slash_is_refused_by_the_lock`
  (the `flock` on `/`'s own inode; a second lock is refused with "another
  ranikust build is using / in this container"); `exposure.rs`, profile
  `sysadmin` (T21): `one_thread_in_another_mount_namespace_listed_late_reaches_its_process`
  (one thread unshared after a hundred others refuses its process) and
  `a_process_of_another_mount_namespace_is_reached_and_its_zombie_is_gone`.
- **T3:** `just build-static`.

## Metrics

- Emits `ranikust.run` (the process), `ranikust.build` (from flag validation
  on), `ranikust.prune` (the `prune` subcommand, `rnk_store.prune` inside),
  `ranikust.setup` (namespace, state directory, store open, probes,
  spawner) and inside it `ranikust.probe` with `steps`, the crates' probes
  that really ran, and `ranikust.probe_userns` with `passes`
  ([rows](../../design/metrics.md#ranikust)).
- `Telemetry::finish` on every way out that returns. `set_meta`:
  `ranikust_version`; for `build`, `mode` and `options`
  (`BuildConfig::meta_options`: counts, booleans and enum names, never a
  destination, path, build argument or credential). The kernel refusal exits
  before `rnk_metrics::init`, so it writes no report.

## Pitfalls

- A thread started before `unshare` (tokio, rayon's pool, a non-blocking log
  writer) silently stays in the host mount namespace
  ([design/sandbox.md](../../design/sandbox.md#process-set-up-once-per-build)).
- Forgetting `#[global_allocator]` is no error, just 250-300x slower
  allocation at 16 threads on musl
  ([exp-05](../../research/experiments/05-rust-ecosystem.md#q2-allocator)).
- mimalloc, the test binary's allocator too, has no fork handler: a test
  that forks and allocates in the child runs itself again alone, with one
  test thread (`alone` in `setup/tests/userns.rs`).
- `rnk_sandbox::Caps` is `#[non_exhaustive]`: `setup::pick` is generic over
  the caps so its tests need no real probe.
- kaniko requires `--destination` unless `--no-push` (`root.go:200-202`),
  rejects zstd with `--tar-path` or `--image-format=docker` (`:177-188`), and
  refuses to run outside a container without `--force` (`:240-245`);
  ranikust builds elsewhere in its state directory instead (ADR-0050).
- kaniko's booleans are pflag's: `--no-push` and `--no-push=false` work,
  `--no-push false` does not (the `false` is an argument). clap matches it
  with `require_equals` and `num_args = 0..=1`.
- clap would report a known kaniko flag as "unexpected argument"; the
  pre-pass in `cli::parse` refuses it by name first. A new kaniko flag needs
  a row in `REFUSED` or the accepted set and in the kaniko map (ADR-0036).
