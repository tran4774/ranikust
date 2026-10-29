# ranikust

> **Status:** skeleton · **Phase:** 1 ·
> **Depends on:** `rnk-metrics` (subscriber, report), `rnk-sched`, `rnk-sandbox` and `rnk-snapshot` (probes), `rnk-store` (`prune`) · **Used by:** — · **Path:** `crates/ranikust/`

**TL;DR:** The binary: subcommands, kaniko-compatible flags, and the process
set-up that must run while single-threaded. Start no thread (tokio, rayon, a
log writer) before `unshare`, the probes and the kaniko-mode workdir lock.

## Owns

- Parsing the `build`, `push`, `prune`, `version` and `login` subcommands,
  accepting the common kaniko flags, plus `--mode` (ADR-0023).
- Process set-up while single-threaded: check the workdir, enter a private
  mount namespace when permitted, run the probes of `rnk-sandbox` and
  `rnk-snapshot` in order and pick the outcome, `flock` the workdir in
  kaniko mode only; then hand off to the libraries. `prune` calls `rnk-store`.

## Must not

- Contain build logic: everything past flag parsing and process set-up lives
  in the library crates ([`main.rs`](../../../crates/ranikust/src/main.rs) `# Boundaries`).
- Keep global state (paths as `rnk-store`'s `BuildPaths`, settings as
  values), probe a capability itself (each crate probes its own), or
  re-implement the workdir checks or stub cleanup [open: Q-011].

## API (planned)

```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;          // [decided] ADR-0020
fn main() -> std::process::ExitCode;   // [design] no #[tokio::main]: runtime built by hand after set-up
enum Command { Build(BuildArgs), Push(PushArgs), Prune(PruneArgs), Version, Login(LoginArgs) } // [decided] ADR-0011
struct BuildArgs { /* kaniko names: context, dockerfile, destination, tar_path, oci_layout_path, no_push,
    cache, cache_repo, build_arg, target, reproducible, compression, registry_certificate, ignore_path,
    snapshot_mode (full|redo|time); ours: workdir, state_dir, mode, jobs, parallel_stages, network,
    seccomp, userns */ } // [design] clap; list fixed in phase 1
enum ModeArg { Auto, Overlay, Kaniko }                       // [decided] --mode, ADR-0023
enum Outcome { OverlayRoot, OverlayRootless, KanikoCanMount, KanikoCannotMount, Refuse(Guidance) }
    // [decided] ADR-0002, ADR-0004; picked here from the crates' probe reports [design]; the type
    // the libraries receive [open: Q-017]
struct PushArgs { path: PathBuf, destination: Vec<String> } // [open: Q-048] kaniko pushes a docker-save tar or OCI layout
```

Set-up order [design]: `rnk_metrics::init` (done; starts no thread) →
validate flags (no I/O) → workdir checks →
`unshare(CLONE_NEWNS)`, `/` rslave → probes of `rnk-sandbox` and
`rnk-snapshot` in forked children → outcome → `rnk-store` open and
clean-up → kaniko mode: `flock` the workdir → spawner thread, stub-journal
replay → tokio runtime → `rnk-sched`.

## Invariants

- Single-threaded until the spawner thread: in a multi-threaded process
  `unshare(CLONE_NEWNS)` moves only the caller; `CLONE_NEWUSER` fails `EINVAL`.
- Release builds abort on panic: nothing reachable from flags or the
  environment may `unwrap`, `expect` or index; no cleanup in `Drop`.
- One static-pie musl binary: no interpreter, no `NEEDED`, no OpenSSL.
- An accepted kaniko flag means what it means in kaniko, or is rejected with a
  message; never accepted and silently different (ADR-0011).

## Refs

- **Design:** [design/sandbox.md#process-set-up-once-per-build](../../design/sandbox.md#process-set-up-once-per-build), [design/modes-and-detection.md#probe-order---modeauto](../../design/modes-and-detection.md#probe-order---modeauto), [#workdir-rules](../../design/modes-and-detection.md#workdir-rules), [#refusal-and-guidance](../../design/modes-and-detection.md#refusal-and-guidance), [design/kaniko-mode.md#host-safety-checks](../../design/kaniko-mode.md#host-safety-checks)
- **Decisions:** [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (flags, subcommands), [ADR-0023](../../adr/0023-mode-flag.md) (`--mode`, `--snapshot-mode`), [ADR-0022](../../adr/0022-common-state-dir.md) (`--state-dir` defaults, workdir lock), [ADR-0021](../../adr/0021-rnk-store-crate.md) (`prune`), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (static musl, mimalloc, rustls), [ADR-0004](../../adr/0004-support-every-privilege-level.md) (refuse with guidance), [ADR-0013](../../adr/0013-jobs-default-available-parallelism.md), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0010](../../adr/0010-run-network-host-or-none.md)
- **Evidence:** [exp-05 Q1](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) (5.1 MB static-pie runs in an empty
  rootfs), [Q2](../../research/experiments/05-rust-ecosystem.md#q2-allocator) (musl 2.8 Mops/s vs 663–753 with mimalloc, 16 threads).
- **kaniko:** `kaniko@44aa7ddc0 cmd/executor/cmd/root.go:336-389` (build
  flags), `:391-410` (registry flags), `:71-119` (env, mirrors), `:177-188`
  and `:200-202` (output checks), `:240-245` (container check), `:595-633`
  (context); `cmd/executor/cmd/push.go:40-80`; `login.go:46-60`
- **External:** <https://docs.rs/clap/latest/clap/>, <https://crates.io/crates/mimalloc>

## Tests

- T0: one test per accepted flag (name, default, repeatable, env variable)
  asserting the config; rejected kaniko flags; refusal of a non-empty
  unmarked `--workdir` under a fresh temp dir. T3: `just build-static`.
- T2 only, in the VM: `--workdir` at `/`, `/home`, other system directories
  ([testing](../../testing.md#planned-test-suites)).
- T0 done: the metrics report on every exit path, panic included
  (`tests/metrics.rs`; hidden `RANIKUST_DEBUG_FAULT=panic|error`).

## Metrics

- Emits `ranikust.run` (done), then `ranikust.build`, `ranikust.setup`,
  `ranikust.probe` ([design/metrics.md](../../design/metrics.md#ranikust)).
- `Telemetry::finish` on every way out that returns. Task 8: `--metrics-file`
  [open: Q-023] fills `Config::file`; `set_meta` records the mode and the
  cost-relevant options, never build arguments or credentials.

## Pitfalls

- A thread started before `unshare` (tokio, rayon's pool, a non-blocking log
  writer) silently stays in the host mount namespace ([design/sandbox.md](../../design/sandbox.md#process-set-up-once-per-build)).
- Forgetting `#[global_allocator]` is no error, just 250–300× slower
  allocation at 16 threads on musl ([exp-05](../../research/experiments/05-rust-ecosystem.md#q2-allocator)).
- `--snapshot-mode` is kaniko's (`full|redo|time`, default `full`,
  `root.go:341`) and keeps that meaning; the mode is `--mode`. In overlay
  mode warn and ignore it (ADR-0023).
- kaniko requires `--destination` unless `--no-push` (`root.go:200-202`),
  rejects zstd with `--tar-path` or `--image-format=docker` (`:177-188`), and
  refuses to run outside a container without `--force` (`:240-245`).
- kaniko reads `KANIKO_REGISTRY_MIRROR`, `KANIKO_NO_PUSH`, `KANIKO_REGISTRY_MAP`
  (`root.go:75-104`) [open: Q-023]; defaults `--context=/workspace/`,
  `--dockerfile=Dockerfile` (`:337-338`); `--kaniko-dir` is `--state-dir`
  (default `<workdir>/.ranikust` or `/ranikust/state`, ADR-0022).

## Phase 1

- `build` with the MVP flags (`--context` as a local directory,
  `--dockerfile`, `--destination`, `--tar-path`, `--oci-layout-path`,
  `--no-push`, `--build-arg`, `--target`, `--reproducible`, `--compression`,
  `--registry-certificate`, `--workdir`, `--state-dir`, `--mode`, `--jobs`,
  `--network=none`, `--metrics-file`) and `version`; fix the final list
  and the `--parallel-stages` name (ADR-0011, ADR-0014).
- Rootful overlay only: elsewhere, fail with the probe's diagnosis.
- Deferred: `--parallel-stages`, `--ignore-path` (phase 2); `--seccomp`,
  `--userns` (3); `--cache`, `--cache-repo` (4); `prune` (5); `push` and
  `login` [open: Q-048].
