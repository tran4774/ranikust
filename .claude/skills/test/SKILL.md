---
name: test
description: How to write and run ranikust tests and keep the machine clean while doing it - which tier a test belongs to, the patterns large Rust CLI projects use (sandboxed temp dirs, scrubbed environment, fake services, normalised output, failure dumps), sharing the VM image cache between worktrees, one VM at a time, and cleaning up downloads and generated files. Use whenever adding or running tests, an e2e or VM run, or any one-off experiment that downloads or generates files.
---

# Testing ranikust

Tiers and commands are in `docs/testing.md`; the change loop is the
`dev-loop` skill, real-root tests are the `vm-test` skill. This skill is
the rules that hold across all of them.

## Pick the lowest tier that can show the bug

| Tier | Where | Use for |
|---|---|---|
| T0 | `cargo test`, `just check` | everything that needs no privilege: parsers, encoders, path handling on temp dirs, the CLI as a black box |
| T1 | `just smoke-case [name]`, `just smoke` (rootless podman) | the binary in a container without root |
| T2 | `just test-vm [unit\|smoke\|quick\|push\|e2e]` | real root, mounts, `pivot_root`, rootful Docker; never on the host |

A T2-only check still gets a T0 test for its logic where one is possible.

## Smoke and proofs of concept: the quick cases, not nextjs

A build to see something work, a smoke test or a proof of concept uses a
small Dockerfile from `tests/smoke/cases/<name>/` (alpine or scratch, a few
seconds per build): `just smoke-case <name>` in rootless podman, `just
test-vm quick` (`RANIKUST_QUICK_CASES="<name> ..."`) as real root. Add a
case there rather than reaching for a big project; one concern per case,
its checks in `case.sh` (helpers in `tests/smoke/cases.bash`).

The nextjs-kaniko workloads (`just test-vm e2e`: W1–W3 with kaniko,
BuildKit and ranikust, a 12 GiB VM, 45–60 minutes for 3 rounds) are only
for a large benchmark or an acceptance measurement. Even then start with
`RANIKUST_E2E_BUILDERS=ranikust RANIKUST_E2E_ROUNDS=1`; run all builders
and 3 rounds only when official numbers are needed.

## Write tests the way large Rust CLIs do

Patterns from cargo, ripgrep, fd, uv and ruff; follow them without new
dependencies:

- **Sandbox per test.** Every test owns a `tempfile::TempDir`, deleted on
  drop, and puts everything there: `--workdir`, `--state-dir`, context,
  outputs, a fake `HOME`/`DOCKER_CONFIG`. Never write to the repo, `/tmp`
  directly or `$HOME`.
- **Black-box the binary** through `env!("CARGO_BIN_EXE_ranikust")`, with
  a scrubbed environment: `env_clear()` or remove `DOCKER_CONFIG`,
  `SOURCE_DATE_EPOCH`, `RANIKUST_*`, `KANIKO_*`, `FF_KANIKO_*`, `NO_COLOR`
  and set only what the test needs (see `crates/ranikust/tests/cli.rs`).
- **No network.** Registries are the fake in `rnk-registry`'s tests or the
  compose registries (`tests/registry`, `just registry-up`); images come
  from the VM cache. A test that needs the internet is a bug.
- **Fix time and order instead of filtering them.** `SOURCE_DATE_EPOCH`,
  `--reproducible`, sorted listings. Where output still carries temp paths
  or timings, normalise them (`[TEMP_DIR]`, `[TIME]`) before comparing.
- **Determinism is a test, not a hope.** Anything that ends up in a layer or
  digest gets a test that runs it at 1 and N threads, or twice, and compares
  bytes (`thread_count_never_changes_the_bytes` is the model).
- **Failures explain themselves.** On a failed CLI assertion print the
  command, cwd, exit code, stdout, stderr and a listing of the sandbox
  (ripgrep's harness). A bare `assert!(status.success())` is not enough.
- **A control for every "cannot happen" check**: show the check would see
  it (the plain-`chroot` escape control in the smoke suite).
- **Heavy tests are serialised, not raced.** One VM at a time (below); a
  test that spawns processes or takes locks goes in its own test binary
  (`crates/rnk-store/tests/processes.rs`).
- **Platform limits are skips with a reason**, in one place: `#[ignore =
  "T2: ..."]` for root, the documented skip list of `just test-aarch64` for
  emulation. Never delete a test to make a run pass.

Not adopted yet (need a maintainer decision because they add a dev-dependency
or a tool): `cargo-nextest` with a `ci` profile (retries, timeouts, a
max-threads-1 group), `insta` snapshots with path filters for CLI output,
`assert_cmd`/`assert_fs`, `cargo-fuzz` targets for the Dockerfile parser,
ignore matcher and tar decoding, `cargo-mutants` on changed pure crates.
Propose one before using it.

## The VM: one shared cache, one VM at a time

- `tests/vm/vm.sh` keeps the verified cloud images and the e2e reference
  images in `tests/e2e/_vm-image/` of the **main checkout** (gitignored).
  Every worktree finds the main checkout through `git rev-parse
  --git-common-dir` and uses that one cache: never point
  `RANIKUST_VM_CACHE` at a worktree copy, never download a second image.
- A run holds `flock` on `_vm-image/.vm.lock` until teardown; a second run
  waits (`RANIKUST_VM_LOCK_WAIT`, default 3600 s). Do not work around the
  lock or boot QEMU yourself. The host has 29 GiB: two VMs (4–12 GiB each)
  push it into swap and hang the e2e VM.
- Before starting a long run, check nothing else is up:
  `pgrep -f '^qemu-system'` (anchor the pattern, or it matches your own
  shell). Run it in the background and wait for its notification.
- After a run, no QEMU, no `target/vm/run.*` and no monitor socket dir
  should be left. If one is, it was not torn down: report it, then stop it.

## Clean up what a test leaves

- **Tests clean up after themselves** through `TempDir` drop; nothing a
  test writes survives it. The VM harness deletes its run dir, disk and key.
- **One-off experiments** (a download to try something, a generated corpus,
  a scratch build, an extracted archive): put them in the session
  scratchpad or a temp dir, and delete them when the experiment is done.
- **Keep what the next run will reuse, delete it at the last run.** If the
  same files will be needed again in this task (a VM image, saved reference
  images, a warmed `CARGO_TARGET_DIR`, a test registry's `_data`), keep
  them and say so in the report; delete them in the run that is known to be
  the last one, and say that too. Never delete a cache another worktree or
  session may be using; when unsure, ask.
- **Large leftovers to check before finishing:** extra
  `CARGO_TARGET_DIR`s (`target/<name>/`, several GB each), `target/vm/`,
  `tests/registry/_data/` (a running compose stack: `just registry-down`),
  podman images and containers you created, the old VM cache
  `~/.cache/ranikust/vm` once nothing uses it.
- Report what was deleted and what was kept and why.
