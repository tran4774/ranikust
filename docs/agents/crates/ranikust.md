# ranikust

> **Status:** in progress · **Phase:** 1 ·
> **Depends on:** `rnk-metrics` (subscriber, report), `rnk-rootfs` (start-up probe), `rnk-sched`, `rnk-sandbox` and `rnk-snapshot` (probes), `rnk-store` (`prune`) · **Used by:** — · **Path:** `crates/ranikust/`

**TL;DR:** The binary: subcommands, kaniko-compatible flags, and the process
set-up that must run while single-threaded. Start no thread (tokio, rayon, a
log writer) before `unshare`, the probes and the kaniko-mode workdir lock.

## Owns

- Parsing the `build`, `push`, `prune`, `version` and `login` subcommands,
  accepting the common kaniko flags, plus `--mode` (ADR-0023). Built:
  `build` (the default) and `version`.
- Process set-up while single-threaded: check the workdir, enter a private
  mount namespace when permitted, run the probes of `rnk-sandbox` and
  `rnk-snapshot` in order and pick the outcome, `flock` the workdir in
  kaniko mode only; then hand off to the libraries. `prune` calls `rnk-store`.
- `BuildConfig`, the validated settings of a build: in the binary
  (`src/config.rs`) until a design assigns it to a library.

## Must not

- Contain build logic: everything past flag parsing and process set-up lives
  in the library crates ([`main.rs`](../../../crates/ranikust/src/main.rs) `# Boundaries`).
- Keep global state (paths as `rnk-store`'s `BuildPaths`, settings as
  values), probe a capability itself (each crate probes its own; the kernel
  check is `rnk_rootfs::probe_support`), or re-implement the workdir checks
  or stub cleanup.

## API

Built items say `built`; the rest is planned.

```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;          // [decided] ADR-0020; task 5a
fn main() -> std::process::ExitCode;   // built; no #[tokio::main]: runtime built by hand after set-up [design]
enum Command { Build(Box<BuildArgs>), Version }                  // built (clap, Q-012); no subcommand = Build
    // Push(PushArgs), Prune(PruneArgs), Login(LoginArgs): later [open: Q-048]
struct BuildArgs { /* built: context, dockerfile, build_arg, target, platform, destination, no_push,
    tar_path, oci_layout_path, compression, compression_level, image_format, label, annotation,
    digest files, insecure*, skip_tls_verify*, push_retry, image_download_retry, verbosity,
    log_format, log_timestamp, reproducible, registry_certificate, snapshot_mode (ADR-0036);
    ours: mode, workdir, state_dir, jobs, network, secret, allow, metrics_file.
    Planned: cache, cache_repo (4), ignore_path (2), parallel_stages (2), seccomp, userns (3) */ }
fn cli::parse(args) -> Result<Parsed, ParseError>;  // refused kaniko flags first; ignored ones
    // taken out and old spellings renamed, each with a warning (Parsed::warnings); then clap
fn config::apply_env(&mut BuildArgs, vars) -> Result<Vec<String>, Invalid>;  // KANIKO_* (ADR-0036)
struct BuildConfig { /* built: the flags validated, paths absolute, enums resolved */ }
impl BuildConfig {
    fn from_args(&BuildArgs) -> Result<Self, Invalid>;       // built: kaniko's checks, no I/O
    fn state_dir_for(&self, Mode) -> Option<PathBuf>;        // built: ADR-0022 defaults
    fn meta_options(&self) -> serde_json::Value;             // built: no name, path or argument
}
enum Mode { Auto, Overlay, Kaniko }                          // built: --mode, ADR-0023
// src/setup.rs, built (task 8): Phase 1 has one outcome; the others refuse.
enum Outcome { OverlayRoot { sandbox: rnk_sandbox::Caps, overlay: OverlayCaps } }
    // planned: OverlayRootless (3), KanikoCanMount, KanikoCannotMount (2) [decided] ADR-0002,
    // ADR-0004; each library receives only its own part [open: Q-017]
// overlay: rnk_snapshot::OverlayCaps (the mount API: lowerdir+ or mount(2))
struct Refusal { reason: String, guidance: &'static str }        // what failed, what to do
struct Setup { outcome: Outcome, state_dir: PathBuf, store: Arc<rnk_store::Store>, sandbox: rnk_sandbox::Sandbox }  // the hand-off
fn set_up(state_dir: &Path, workdir: Option<&Path>) -> Result<Setup, Refusal>;  // namespace → state dir → store → probes → outcome → spawner
fn prepare_state_dir(state_dir, workdir, euid) -> Result<(), Refusal>; // 0700; no symlink, owner euid, no g/o write up to the workdir
fn probe_overlay(build_dir: &Path, euid: u32) -> Result<OverlayCaps, Refusal>; // rnk_snapshot::probe in builds/<id>/overlay-probe
struct PushArgs { path: PathBuf, destination: Vec<String> } // [open: Q-048] kaniko pushes a docker-save tar or OCI layout
```

Set-up order, built so far: parse the flags (no I/O; refused kaniko flags
reported first, so `--help` works on any kernel) → the Linux 5.8 check
(`rnk_rootfs::probe_support`, ADR-0028; skipped by `version`) →
`rnk_metrics::init` (starts no thread), `--metrics-file` over
`--metrics-file` (ADR-0037) → `BuildConfig::from_args` → `--mode=kaniko`:
without `--workdir` it stops (Phase 2); with it, `workdir::claim` (the
checks and lock of `ADR-0038`, below) and `raise_nofile_limit`, then it
stops with exit code 2 → the overlay-mode state directory, `--state-dir` or
`<workdir>/.ranikust`, else a usage error [open: Q-110] → `setup::set_up`:
`rnk_sandbox::enter_build_namespace` (refuses in a plain `chroot`, then
`unshare(CLONE_NEWNS)`, `/` rslave) → `prepare_state_dir` (created 0700;
refused if it or a directory above it up to the workdir is a symlink, not
owned by the effective uid, or group/world-writable) → `rnk_store::Store::open`
(marker, start-up clean-up; (Q-111) interim (a)) →
`rnk_sandbox::probe` in `<state-dir>/sandbox` → `rnk_snapshot::probe` in
`builds/<id>/overlay-probe` → outcome → `Sandbox::start` (stub-journal
replay, spawner thread) → the store is closed → stop with exit code 2,
"set up for …, but building is not implemented yet". Still to
come [design]: `rnk-store` open and clean-up before the spawner (task 7) (Q-111) →
kaniko mode: its state directory, probes and outcome after the workdir
lock (Phase 2 task 2) → tokio runtime → `rnk-sched`.

Kaniko-mode workdir checks (`src/workdir.rs`, `ADR-0038`), before anything
is written: canonicalize; refuse `/` and any of the 30 `SYSTEM_DIRS` or
their ancestors, by name and by `(dev, ino)` of the opened workdir against
each system directory and its ancestors; `RootPath::open`
(`RESOLVE_NO_SYMLINKS`); `flock(LOCK_EX | LOCK_NB)` on a readable fd held
by `Workdir` for the build; a non-empty workdir without a regular
`.ranikust-root` refused. `claim` writes nothing: an empty workdir gets the
marker from `Workdir::mark` right before the build first writes to it,
which lists the workdir again and drops the marker if anything else
appeared. Everything done in the workdir later, deletion above all, goes
through `Workdir::root()`, never a reopened `path()`. `RLIMIT_NOFILE`'s
soft limit is raised to the hard limit; `RUN` children must get the
original back (Phase 2 task 7).

Exit codes: 0 success (`--help`, `--version`, `version` included); 1 a
runtime error, an unsupported kernel or a refused environment; 2 a usage
error, a refused kaniko flag, an invalid combination, and for now every
`build` that gets through the set-up.

## Invariants

- Single-threaded until the spawner thread: in a multi-threaded process
  `unshare(CLONE_NEWNS)` moves only the caller; `CLONE_NEWUSER` fails `EINVAL`.
- Release builds abort on panic: nothing reachable from flags or the
  environment may `unwrap`, `expect` or index; no cleanup in `Drop`.
- One static-pie musl binary: no interpreter, no `NEEDED`, no OpenSSL.
- An accepted kaniko flag means what it means in kaniko, or is rejected with a
  message; never accepted and silently different (ADR-0011). A known kaniko
  flag that is not accepted is refused by name, with the reason
  (`REFUSED` in `src/cli.rs`), never left to clap's "unexpected argument".

## Refs

- **Design:** [design/sandbox.md#process-set-up-once-per-build](../../design/sandbox.md#process-set-up-once-per-build), [design/modes-and-detection.md#probe-order---modeauto](../../design/modes-and-detection.md#probe-order---modeauto), [#workdir-rules](../../design/modes-and-detection.md#workdir-rules), [#refusal-and-guidance](../../design/modes-and-detection.md#refusal-and-guidance), [design/kaniko-mode.md#host-safety-checks](../../design/kaniko-mode.md#host-safety-checks)
- **Decisions:** [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (flags, subcommands), [ADR-0023](../../adr/0023-mode-flag.md) (`--mode`, `--snapshot-mode`), [ADR-0022](../../adr/0022-common-state-dir.md) (`--state-dir` defaults, workdir lock), [ADR-0028](../../adr/0028-minimum-kernel-linux-5-8.md) (start-up check), [ADR-0021](../../adr/0021-rnk-store-crate.md) (`prune`), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (static musl, mimalloc, rustls), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (refuse with guidance), [ADR-0013](../../adr/0013-jobs-default-available-parallelism.md), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0010](../../adr/0010-run-network-host-or-none.md)
- **Evidence:** [exp-05 Q1](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) (5.1 MB static-pie runs in an empty
  rootfs), [Q2](../../research/experiments/05-rust-ecosystem.md#q2-allocator) (musl 2.8 Mops/s vs 663–753 with mimalloc, 16 threads).
- **kaniko:** `kaniko@44aa7ddc0 cmd/executor/cmd/root.go:336-389` (build
  flags), `:391-410` (registry flags), `:71-119` (env, mirrors), `:177-188`
  and `:200-202` (output checks), `:240-245` (container check), `:595-633`
  (context); `cmd/executor/cmd/push.go:40-80`; `login.go:46-60`
- **Flag status:** [kaniko-map.md#flags](../kaniko-map.md#flags), column "Now".
- **External:** <https://docs.rs/clap/latest/clap/>, <https://crates.io/crates/mimalloc>

## Tests

- T0 built, `src/cli/tests.rs`: kaniko spellings per accepted flag
  (short forms, `=` and separate values, pflag booleans, `--target` lists),
  ranikust's flags, refused flags with their reasons (all reported at once,
  `-b`, nothing after `--`), clap errors. `src/config/tests.rs`: kaniko's
  checks, contexts, build arguments, absolute paths, state-dir defaults,
  `meta_options` leaking no destination, path or argument.
- T0 built, `tests/cli.rs` runs the binary: exit codes of `--help`,
  `version`, refusals, validation and the stub build; the kernel refusal
  and the seccomp message through `RANIKUST_DEBUG_FAULT=kernel-missing` and
  `kernel-blocked`; `--metrics-file` over the variable. `tests/metrics.rs`: the report on
  every exit path, panic included (`RANIKUST_DEBUG_FAULT=panic|error`).
- T0 built, `src/setup/tests.rs`: the outcome from the probes' results
  (both succeed; no privileges, as root and not; a failed step reported
  verbatim with kaniko mode as the way; a host-side failure without it; a
  failed overlay probe), the plain-chroot refusal's message, and the
  state-directory check (created 0700; a symlinked state dir or workdir,
  a group- or world-writable one, another owner refused, nothing created
  through a symlink; only the chain up to the workdir checked).
  `tests/cli.rs`: overlay mode without `--workdir`/`--state-dir`,
  `--mode=kaniko`, and, unless the tests run as root, the refusal before
  the namespace with the workdir left empty and `ranikust.setup` in the
  report.
- T1 built, `just smoke`, and T2 built, `just test-vm smoke`: the release
  binary's set-up in rootless podman, on the VM as root, in Docker
  `--privileged` (`pivot_root`) and with `SYS_ADMIN` under Docker's seccomp
  (`MS_MOVE`); refused inside a plain `chroot` (`tests/smoke/lib.bash`
  `smoke_check_setup`).
- T0 built, `src/workdir/tests.rs`: `/`, `//`, `/usr/../` and system
  directories refused, a symlink to `/usr` refused by its target, an empty
  workdir locked (a second claim refused) and marked only by `mark`, a
  workdir filled before `mark` refused with the marker removed, a non-empty
  one refused without the marker and nothing written, the name and
  identity checks as pure functions, a marker that is a
  symlink or directory refused, `RLIMIT_NOFILE` at the hard limit.
  `tests/cli.rs`: `--mode=kaniko --workdir` refused when unmarked, stopped
  with nothing written when empty, `/usr` refused. T3: `just build-static`.
- T2 only, in the VM: `--workdir` at `/`, `/home`, other system directories
  ([testing](../../testing.md#planned-test-suites)).

## Metrics

- Emits `ranikust.run` (the process), `ranikust.build` (from flag
  validation on), `ranikust.workdir` (kaniko mode's workdir checks and
  lock), `ranikust.setup` (namespace, store open, probes,
  spawner) and inside it `ranikust.probe` with `steps`, the crates'
  probes that really ran (the sandbox's and the overlay probe)
  ([design/metrics.md](../../design/metrics.md#ranikust)).
- `Telemetry::finish` on every way out that returns. `--metrics-file` fills
  `Config::file`. `set_meta`: `ranikust_version`; for `build`, `mode` and
  `options` (`BuildConfig::meta_options`: counts, booleans and enum names,
  never a destination, path, build argument or credential). The kernel
  refusal exits before `rnk_metrics::init`, so it writes no report.

## Pitfalls

- A thread started before `unshare` (tokio, rayon's pool, a non-blocking log
  writer) silently stays in the host mount namespace ([design/sandbox.md](../../design/sandbox.md#process-set-up-once-per-build)).
- Forgetting `#[global_allocator]` is no error, just 250–300× slower
  allocation at 16 threads on musl ([exp-05](../../research/experiments/05-rust-ecosystem.md#q2-allocator)).
- `--snapshot-mode` is kaniko's (`full|redo|time`, default `full`,
  `root.go:341`) and keeps that meaning; the mode is `--mode`. In overlay
  mode warn and ignore it (ADR-0023): built, after the set-up picked
  overlay mode.
- `rnk_sandbox::Caps` is `#[non_exhaustive]`: `setup::pick` is generic
  over the caps so its tests need no real probe.
- The sandbox probe creates `<state-dir>/sandbox` before `rnk-store` has
  checked the state directory's marker (Q-111).
- kaniko requires `--destination` unless `--no-push` (`root.go:200-202`),
  rejects zstd with `--tar-path` or `--image-format=docker` (`:177-188`), and
  refuses to run outside a container without `--force` (`:240-245`).
- kaniko's booleans are pflag's: `--no-push` and `--no-push=false` work,
  `--no-push false` does not (the `false` is an argument). clap matches it
  with `require_equals` and `num_args = 0..=1`.
- clap would report a known kaniko flag as "unexpected argument"; the
  pre-pass in `cli::parse` refuses it by name first (`REFUSED`), takes out
  the ignored ones (`IGNORED`, with whether they take a value, so the next
  argument goes with them) and renames old spellings (`RENAMED`). A new
  kaniko flag needs a row there and in the kaniko map (ADR-0036).
- kaniko's variables are applied by `config::apply_env` before validation:
  `KANIKO_NO_PUSH` overrides the flag, `KANIKO_REGISTRY_MIRROR`/`_MAP` exit
  2, the rest get one warning (`root.go:75-104`, ADR-0036); defaults `--context=/workspace/`,
  `--dockerfile=Dockerfile` (`:337-338`); `--kaniko-dir` is `--state-dir`
  (default `<workdir>/.ranikust` or `/ranikust/state`, ADR-0022).

## Phase 1

- `build` with the MVP flags (`--context` as a local directory,
  `--dockerfile`, `--destination`, `--tar-path`, `--oci-layout-path`,
  `--no-push`, `--build-arg`, `--target`, `--reproducible`, `--compression`,
  `--registry-certificate`, `--workdir`, `--state-dir`, `--mode`, `--jobs`,
  `--network=none`, `--metrics-file`) and `version`; fix the final list
  and the `--parallel-stages` name (ADR-0011, ADR-0014).
- Done (task 8a): those flags and `--platform` parsed and validated into
  `BuildConfig`, the other kaniko flags refused with a reason, the Linux
  5.8 check, `meta.mode` and `meta.options`.
- Done (task 8b): the mount namespace, the sandbox probe, the outcome
  (overlay mode as root), refusals with guidance, the spawner thread.
- Done (task 9): the store opened before the probes, the overlay probe,
  the store closed at the end.
- Done (task 11): `build.rs` builds after the set-up: Dockerfile and stage
  graph, the context walk with `.dockerignore`, a tokio runtime started
  after the single-threaded set-up, `rnk-sched` over a `StageRunner` that
  pulls bases and `COPY --from` images up front (on a blocking thread:
  the registry client's futures are not `Send` for every lifetime) and
  builds each stage on a thread of its own, which asks the scheduler for
  a `--jobs` permit per `RUN` over a channel; then the outputs in
  kaniko's order (OCI layout, Docker tar, push) and the digest on stdout.
  Roots are released children first, then the store closed, on success
  and failure alike. `--secret` takes buildctl's and kaniko's syntax
  (`id=`, `type=file|env`, `src=`, `env=`; without a source the variable
  named by the id, else the file), ids unique; a file secret must exist
  and fit 500 KiB at the start. `--allow network.host` is BuildKit's
  entitlement (ADR-0032).
  The roots of `COPY --from=<image>` are made by the stage's thread, not
  the preparation, which only pulls. A Dockerfile not found as given is
  read inside the context through its `RootPath`, never by a host path.
  The first SIGINT or SIGTERM, or a failed stage, cancels the build and
  kills the running `RUN`s (`Sandbox::cancel`, as BuildKit); a second
  signal exits at once. A release that fails leaves the stages below it to the
  next build's clean-up.
- Left for Phase 2 ([phase-2.md](../phase-2.md)): the workdir checks and
  the kaniko-mode workdir lock, owned here (`ADR-0038`, Q-011); built in
  Phase 2 task 1.
- Rootful overlay only: elsewhere, fail with the probe's diagnosis.
- Deferred: `--parallel-stages`, `--ignore-path` (phase 2); `--seccomp`,
  `--userns` (3); `--cache`, `--cache-repo` (4); `prune` (5); `push` and
  `login` [open: Q-048].
