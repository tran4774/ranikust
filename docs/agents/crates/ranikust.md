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

- Parsing the subcommands and flags: `build` (the default) and `version` are
  built; `push` (an OCI layout only, ADR-0052) and `prune` (Phase 5; a minimal
  `prune --all` in Phase 3, ADR-0105) are later; there is no `login` (ADR-0049).
  Kaniko's common flags plus `--mode` (ADR-0011, ADR-0023).
- Process set-up while single-threaded: a private mount namespace when
  permitted, the state directory's checks (`statedir`, ADR-0050), the probes
  of `rnk-sandbox` and `rnk-snapshot` in order, the outcome; in kaniko mode
  the root (`/` in a container, `flock`ed; else a directory per stage,
  `KanikoStages`, ADR-0059). Then the hand-off to the libraries. `prune`
  only calls `rnk-store`.
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
| `cli.rs` | `Command { Build(Box<BuildArgs>), Version }` (no subcommand = `Build`), `parse`, `REFUSED` |
| `config.rs` | `BuildConfig::from_args` (kaniko's checks, no I/O), `meta_options`, `overlay_mode_flags`, `Mode { Auto, Overlay, Kaniko }` |
| `setup.rs` | `enter_namespace`, `set_up` (overlay), `set_up_kaniko`, `Outcome`, `Refusal`, `Kind`, `Route`/`route`, `host_without_mounts`, `store_refusal`, `probe_overlay` |
| `statedir.rs` | `prepare` (ADR-0050 checks), `SYSTEM_DIRS`, `on_overlayfs`, `raise_nofile_limit` |
| `container.rs` | `detect`: kaniko's `GetContainerRuntime`, ported (ADR-0040, ADR-0048) |
| `exposure.rs` | warnings for secret-looking `--build-arg` values and `DOCKER_AUTH_CONFIG`, `proc_sys_read_only` |
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
  Planned: `--seccomp`, `--userns` (Phase 3). Ours: `--mode`, `--state-dir`,
  `--jobs`, `--network`, `--secret`, `--allow`, `--limit`, `--metrics-file`,
  `--snapshot-workers`.
- `--snapshot-mode` is kaniko's (`full|redo|time`) and keeps that meaning in
  kaniko mode; the mode is `--mode`. Overlay mode warns and ignores it
  (ADR-0023). In overlay mode `overlay_mode_flags` refuses kaniko flags that
  change the image and warns for the rest (ADR-0049).
- `Outcome` has one variant, `OverlayRoot { sandbox: Caps, overlay:
  OverlayCaps }`; kaniko mode is set up by `set_up_kaniko`
  (`KanikoSetup`). Planned: an overlay-rootless outcome (Phase 3, #41); each
  library receives only its own part [open: #3].
- `Refusal { kind, reason, guidance }`: what failed, what to do.
  `Refusal::needs_kaniko_mode()` lets auto pick kaniko mode.
- `statedir::prepare(state_dir)`: the nearest existing directory and every
  real parent walked by descriptor, owned by root or the user and not
  writable by group or others (root's sticky directories excepted); created
  `0700` by `mkdirat` below the checked descriptor; chain checked again.
  Details: [design/kaniko-mode.md#host-safety-checks](../../design/kaniko-mode.md#host-safety-checks).

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
   which starts over at step 0; the copy's run logs the way at debug after
   step 3, or a warning when no way worked (the build goes on).
   `RANIKUST_DEBUG_FAULT=seal-tmpfile` skips the memfd, `seal-none` both
   ways.
3. `--metrics-file` refused in the build context or through a symlink
   (`check_metrics_file`, ADR-0076); `rnk_metrics::init` (no thread);
   `--metrics-file` over `RANIKUST_METRICS_FILE` (ADR-0037).
4. `BuildConfig::from_args`, then the mode (ADR-0040): `--mode=kaniko` goes
   to kaniko mode; `--mode=overlay` checks the kaniko-mode flags and its
   state directory, then enters the namespace; `--mode=auto` enters the
   namespace first and goes to kaniko mode where it is refused to root for
   lack of privilege (`needs_kaniko_mode`), or where overlayfs cannot use the
   state directory outside a container (`setup::route` on the refusal's
   `Kind`: a failed sandbox step or a probe mount that never reached the
   kernel never routes there; per-stage directories, `can_mount`).
5. **Overlay mode** (`setup::set_up`): `enter_namespace`
   (`rnk_sandbox::enter_build_namespace`: refuses in a plain `chroot`, then
   `unshare(CLONE_NEWNS)`, `/` rslave) → `statedir::prepare`, refused on
   overlayfs (a container's root: mount a volume) → `rnk_store::Store::open`
   (marker, start-up clean-up) → `rnk_sandbox::probe` in
   `<state-dir>/sandbox` → `rnk_snapshot::probe` in
   `builds/<id>/overlay-probe` → outcome → `Sandbox::start` (journal replay,
   spawner thread).
6. **Kaniko mode** (`kaniko` in `main.rs`, then `setup::set_up_kaniko`,
   ADR-0050): `NofileLimit::current()` is read first (for `RUN`); root is
   required; the root is picked by `container::detect`. On `/`,
   `refuse_unsafe_slash` runs in `main.rs`, before anything is written: `/`
   is refused where root may mount (ADR-0054), with `--network=none`
   (ADR-0057), with a writable `/proc/sys`
   (`exposure::proc_sys_read_only`, ADR-0054) and with ranikust's binary on
   a writable mount other than `/`'s, reached by one (its tree bound
   read-write too, another hard link; in a root that is not overlayfs, a
   binary in the root too), or one whose mount cannot be told
   (`refuse_writable_binary` on `Sealing::binary()`, ADR-0101: qemu-user, a
   memfd, a mount table that does not place it or a failed call, each with
   its own guidance); then the
   warnings for secret-looking build arguments and file secrets.
   `RLIMIT_NOFILE` is raised (`statedir::raise_nofile_limit`); on `/`, with
   `--jobs` a warning that stages run one at a time and `--jobs` only
   bounds the uploads, and `rnk_sandbox::block_stop_signals`. Then
   `set_up_kaniko`: off `/`, a host where root may not mount is refused
   (`host_without_mounts`). On `/`, `trim_to_run_capabilities` first,
   while single-threaded, and it stays
   before `Store::open_anonymous`, which deletes names a `RUN` can reach
   (ADR-0078); then the `flock` on `/` itself (one build per container), the
   state directory checks, `Store::open_anonymous` (a state directory holding
   a persistent store or held by another build is refused with the store's
   own message, `store_refusal`, ADR-0077) and `Sandbox::start_direct`
   (ADR-0051); after the trim, `exposure::warn_credential_holders`. Off `/`:
   the state directory, `Store::open`, the sandbox probe and
   `Sandbox::start` (`RUN` in the full sandbox, `--jobs` honoured); when
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
  `/var/lib/ranikust`, ADR-0050).

## Refs

- **Design:** [design/modes-and-detection.md](../../design/modes-and-detection.md) ([probe order](../../design/modes-and-detection.md#probe-order---modeauto), [build root and state directory](../../design/modes-and-detection.md#build-root-and-state-directory), [refusal and guidance](../../design/modes-and-detection.md#refusal-and-guidance)); [design/sandbox.md#process-set-up-once-per-build](../../design/sandbox.md#process-set-up-once-per-build); [design/kaniko-mode.md#host-safety-checks](../../design/kaniko-mode.md#host-safety-checks)
- **Decisions:** [ADR-0050](../../adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md) (the build root, the state directory), [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (flags, subcommands), [ADR-0023](../../adr/0023-mode-flag.md) (`--mode`, `--snapshot-mode`), [ADR-0022](../../adr/0022-common-state-dir.md) (`--state-dir` defaults), [ADR-0028](../../adr/0028-minimum-kernel-linux-5-8.md) (start-up check), [ADR-0021](../../adr/0021-rnk-store-crate.md) (`prune`), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (static musl, mimalloc, rustls), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (refuse with guidance), [ADR-0013](../../adr/0013-jobs-default-available-parallelism.md), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0010](../../adr/0010-run-network-host-or-none.md), [ADR-0098](../../adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md) (planned, Phase 3: a build in a user namespace, one ranikust creates or one it is started in, refuses an image with an owner the namespace does not map, reading the map from `/proc/self/uid_map` and `gid_map` in the second case), [ADR-0100](../../adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) (planned, Phase 3: where the probe is tested), [ADR-0103](../../adr/0103-a-default-seccomp-profile-applies-wherever-the-full-sandbox-runs.md) (planned, Phase 3: a default seccomp profile, the `--seccomp` flag; [#82](https://github.com/tran4774/ranikust/issues/82)), [ADR-0104](../../adr/0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md) (planned, Phase 3: `--userns`, root's range read from a `ranikust` entry of `/etc/subuid` and `/etc/subgid`; [#83](https://github.com/tran4774/ranikust/issues/83)), [ADR-0105](../../adr/0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md) (planned, Phase 3: a minimal `prune --all`, refused when root runs it), [ADR-0107](../../adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md) (planned, Phase 3: routing in a user namespace ranikust did not create)
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
  checks, contexts, build arguments, absolute paths, state-dir defaults,
  `meta_options` leaking no destination, path or argument, the overlay-mode
  refusals and warnings, whole-root `--ignore-path` refused), `setup` (the
  outcome from the probes' results, which refusals route `--mode=auto` to
  kaniko mode, a probe failure other than a refused mount never routes, no
  guidance sends a user back to the mode they used), `statedir` (each
  ADR-0050 refusal: `/`, system directories, symlinks, writable parents,
  nothing created through a planted link; `0700`), `container` (every signal
  of kaniko's container check on a fake host, in kaniko's order).
  `src/main.rs` tests: `kaniko_mode_on_slash_refuses_a_binary_on_a_writable_mount`
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
  `--metrics-file` over the variable; the report on every exit path, panic
  included (`RANIKUST_DEBUG_FAULT=panic|error`); a refused flag exits 2 before
  any work (no state directory, the value never echoed, no `RANIKUST_*`
  variable read). `kaniko_mode_needs_root` and
  `without_root_the_set_up_refuses_before_touching_the_state_dir` run only
  as a normal user (skipped as root): nothing is created, the state
  directory is left empty and `ranikust.setup` is in the report.
- **T1** (`just smoke`, `RANIKUST_CASE_MODE=kaniko just smoke-case`): the
  release binary's set-up and builds in rootless podman (overlay as root in
  a user namespace; kaniko mode on the container's `/`, `RUN` `Direct`);
  the checks of `smoke_checks` in `tests/smoke/lib.bash`; refused inside a
  plain `chroot`. Cases: `tests/smoke/cases/`; `kaniko-flags` tests the
  meaning of `--snapshot-mode` and `--ignore-path` (in overlay mode
  `--ignore-path` is refused and `--snapshot-mode` changes nothing) and, in
  the kaniko leg (a default container), `--mode=auto` picking kaniko mode.
- **T2** (VM; `just test-vm smoke quick`, `RANIKUST_QUICK_MODE=kaniko` for
  kaniko mode; `tests/vm/guest/quick.sh`): set-up as root, on `--privileged`
  Docker (`pivot_root`) and `SYS_ADMIN` under Docker's seccomp (`MS_MOVE`);
  all quick cases in both modes; `rt_*` checks (`container=docker` on the VM
  never builds on `/`, `--mode=auto` with `--ignore-path` refused after
  overlay mode's set-up leaves nothing, a ramfs state directory refused, root
  without `CAP_SYS_ADMIN` refused before the state directory exists);
  `slash_lock_refuses_a_second_build`, whose first build on `/` must run
  from a sealed memfd (ADR-0101);
  `adr0054_refusals` (kaniko mode on `/` refused with `--privileged` and
  with `systempaths=unconfined`, before anything is pulled);
  `binary_mounts_on_slash` (ADR-0101: the binary bound read-write, file or
  directory, refused with the `:ro` guidance before the state directory
  exists, and so are the binary bound `:ro` from a tree bound read-write too
  and a file with another hard link; bound `:ro`, its tree `:ro` too, or
  copied into the container's root, it builds; the
  only test of `refuse_unsafe_slash` calling `refuse_writable_binary`);
  `rt_auto_privileged_container` (`--mode=auto` in a `--privileged`
  container is refused, with or without `/.dockerenv`). A
  `--state-dir` at `/`, below `/home/<user>` or in a directory others may
  write ([testing](../../testing.md#planned-test-suites)).
- **T2c:** `setup/tests.rs` `a_second_build_on_slash_is_refused_by_the_lock`
  (the `flock` on `/`'s own inode; a second lock is refused with "another
  ranikust build is using / in this container").
- **T3:** `just build-static`.

## Metrics

- Emits `ranikust.run` (the process), `ranikust.build` (from flag validation
  on), `ranikust.setup` (namespace, state directory, store open, probes,
  spawner) and inside it `ranikust.probe` with `steps`, the crates' probes
  that really ran ([rows](../../design/metrics.md#ranikust)).
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
