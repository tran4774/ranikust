# Workflow

**TL;DR:** Check narrow then wide: `cargo test -p <crate> <filter>`, then
`just check` before calling anything done, `just ci` before a PR. T1/T2 never
run on the host. Conventional Commits; the PR title becomes the commit.

## Setup

Linux only; nothing needs root. `rustup` reads `rust-toolchain.toml` (Rust
1.99.1 = `rust-version` in `Cargo.toml`, bump both; rustfmt, clippy,
rust-analyzer, x86_64 and aarch64 musl targets). Then, at CI's versions:
`cargo install --locked just cargo-deny@0.20.2 mdbook@0.5.4`, plus
`git-cliff` for release notes and `file`/`readelf` for `just build-static`.
The musl recipes (`just build-static`, `just check-aarch64`,
`just test-aarch64`, so `just ci`)
need zig 0.16.0 and `cargo install --locked cargo-zigbuild@0.24.0`: zig
compiles the C dependencies (libzstd, mimalloc, ring) for musl, the host
`cc` links (`ADR-0020`); install zig as in
[CONTRIBUTING.md](../../CONTRIBUTING.md#development-setup).
`just test-aarch64`, so `just ci`, also needs `qemu-aarch64` 9.1 or later
(`qemu-user`, or `QEMU_AARCH64=<path>`).

## Dev loop

1. Edit; a PostToolUse hook runs rustfmt on each `.rs` file you touch.
2. `cargo check -p <crate>` → `cargo test -p <crate> <filter>` →
   `cargo clippy -p <crate> --all-targets -- -D warnings`.
3. `just check` before reporting done; quote the command and its summary.
4. When relevant: `just check-aarch64`, then `just test-aarch64`
   (syscalls, `cfg`, FFI, integer widths), `just deny` (dependencies),
   `just doc` (public items, doc comments), `just book` (`docs/book/`),
   `just ci` before a PR.

Fix at the narrowest level that fails, then widen. Bug fix: test first, see
it fail. After three failed fixes, stop and report. More: `dev-loop` skill.

## Recipes

| Recipe | Runs | CI job |
|---|---|---|
| `just check` | `fmt-check` + `lint` + `test` | |
| `just fmt` / `fmt-check` | `cargo fmt --all` (`--check`) | `fmt` |
| `just lint` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | `clippy` |
| `just test [args]` | `cargo test --workspace --locked [args]` (T0) | `test` |
| `just build-static` | release musl build of `ranikust` with `cargo zigbuild`, linked by `cc`; fails on a loader, `NEEDED`, non-PIE or no `.symtab`; runs `ranikust version` | `static-build` |
| `just check-aarch64` | `cargo-zigbuild check --workspace --locked --target aarch64-unknown-linux-musl` | `check-aarch64` |
| `just test-aarch64 [args]` | static aarch64 musl release build (no loader, no `NEEDED`; runs `ranikust version`), then the T0 tests: natively on an aarch64 host (CI's `ubuntu-24.04-arm`), else under qemu-user through cargo's target runner, skipping three tests qemu cannot pass and, without a binfmt entry, ranikust's `cli` and `metrics` tests ([testing](../testing.md#aarch64-under-qemu-user)) | `test-aarch64` |
| `just deny` | `cargo deny --locked check` | `deny` |
| `just doc` / `book` | rustdoc with `-D warnings` / `mdbook build docs/book` | `docs` |
| `just changelog [args]` | `git cliff --unreleased`, a draft to stdout | |
| `just smoke [image]` | T1 smoke test until `rnk-builder` exists: the `smoke-build` example pulls, extracts, runs `RUN`s in the sandbox inside rootless `podman run --privileged` and writes a one-layer image podman then runs ([testing](../testing.md#smoke-test-until-the-builder-exists-tier-t1)) | |
| `just registry-up`, `just registry-down` | the HTTP and HTTPS test registries on the host's loopback, data in `tests/registry/_data` ([testing](../testing.md#test-registries)) | |
| `just smoke-case [name]...` | T1: the quick smoke cases of `tests/smoke/cases` (small Dockerfiles, one concern each) built by the `ranikust` binary in rootless podman; the default for a smoke test or proof of concept ([testing](../testing.md#quick-smoke-cases)) | |
| `just test-vm [suite]...` | T2 in a disposable QEMU/KVM VM, as the user: `unit` (the `T2:` ignored tests as root) and `smoke` (`smoke-build` as real root and in rootful Docker) by default; `quick` when named (the quick smoke cases as real root); `e2e` when named, for large benchmarks only (the `nextjs-kaniko` workloads against kaniko and BuildKit, [testing](../testing.md#end-to-end-workloads-e2e)); `push` when named (needs `just registry-up`, [testing](../testing.md#test-registries)); results in `target/vm/results/<release>/` ([testing](../testing.md#the-vm-harness-tier-t2)) | |
| `just docs-lint` | `scripts/docs-lint.py`: every ADR indexed with its status, no duplicate question, no `[open: Q-NNN]` left for an answered question | local only for now |
| `just review-pack <branch>` | `scripts/review-pack.py`: the brief for the `host-safety` subagent (changed files by crate, diff stat, `graphify affected`, threat-model rows, design sections, crate boundaries) | local only |
| `just ci` | all of the above that CI runs, in CI's order | all |

`just` lists them. A recipe that mirrors CI runs exactly the job's command:
change both together. `just test -p <crate>` still tests the whole workspace
(`--workspace` wins); for one crate use `cargo test -p <crate>`.

## Test tiers

| Tier | What | Privileges | Where | Status |
|---|---|---|---|---|
| T0 | Unit and in-process tests | none | anywhere: `just test` | in use |
| T1 | User namespaces, rootless overlay, sandbox in a user namespace, Podman | user with `/etc/subuid` entries | developer machine; `just smoke`; `just test-rootless` TODO | smoke test only |
| T2 | Overlay as root, `pivot_root`, `mknod`, Docker variants, kaniko mode on `/`, host-safety attacks | root | disposable VM only: `just test-vm` | `unit`, `smoke`, `e2e` and `push` suites |
| T3 | fmt, clippy, T0, static build, aarch64 check, aarch64 T0 under qemu-user, deny, docs | none | GitHub Actions: `just ci` | in use |

Planned suites: [`docs/testing.md`](../testing.md#planned-test-suites).

## Add a crate

Agree first: it changes `ARCHITECTURE.md` (code map, dependency direction).
Copy an existing `crates/rnk-*/Cargo.toml` (`*.workspace = true`,
`publish = false`, `[lints] workspace = true`); `crates/*` joins by itself.
`src/lib.rs` needs module docs with a `# Boundaries` section. Internal
dependencies go through `[workspace.dependencies]`, then
`<name>.workspace = true`. Add a card from [crates/_template.md](crates/_template.md)
and a row in [README.md](README.md).

## Add a dependency

- Justify it in the PR; start from survey 03 §3 and `exp-05` ([refs.md](refs.md#crates)).
  Commit `Cargo.lock`; recipes run `--locked`.
- `just deny` allows only `deny.toml`'s licenses; MPL-2.0, OpenSSL
  (advertising clause) or anything unlisted needs a maintainer decision
  first. crates.io only, no git sources, no wildcard versions.
- aarch64 musl and a static binary: no OpenSSL, pure-Rust fallback for
  assembly, C code that cross-compiles through zig (`ADR-0009`,
  `ADR-0020`). Run `just check-aarch64`, `just test-aarch64` and
  `just build-static`.

## Add an ADR

The user runs `/adr <title> [supersedes NNNN]` (the skill cannot start
itself; suggest it): next number, `docs/adr/template.md`, status `proposed`,
index row. By hand: [`docs/adr/README.md`](../adr/README.md#adding-an-adr).
Never edit an accepted ADR's body; supersede it.

## Commits

Start: `git switch main && git pull --ff-only && git switch -c <type>/<scope>-<topic>`.
Commit at every checkpoint (builds, `just check` passes, docs updated); see
[rules.md](rules.md#branches-and-commits).

`<type>(<scope>): <imperative summary, lower case, no full stop>`; body at 72
columns; footers `BREAKING CHANGE:`, `Refs: #123`. `feat`, `fix`, `perf` go
into release notes; `refactor`, `test`, `docs`, `ci`, `build`, `chore`,
`style` do not. Scope: a crate or an area (`ci`, `book`, `adr`); `security`
for security fixes; `!` for breaking changes. Truthful `Co-authored-by`.

## Pull requests

Squash-merged, so the title is the commit message. Template checklist:

- [ ] Summary for a reader who never saw your work; issue linked.
- [ ] Test plan: commands, results, tiers run and not run, modes affected.
- [ ] `just ci` passes; a bug fix has a test that fails without it.
- [ ] Docs in sync ([rules.md](rules.md#keep-in-sync)); `/sync-docs` run.
- [ ] Ported code has a file header and a `NOTICE` entry.
- [ ] AI use disclosed; every line reviewed, understood and tested.
- [ ] Optional: "Suggested agent rule additions".

## Keep in sync

The matrix lives in [rules.md](rules.md#keep-in-sync).

## Claude Code

| When | Use |
|---|---|
| Changing code in `crates/` | `dev-loop` skill |
| What kaniko or BuildKit does | `kaniko-parity` or `buildkit-parity` skill; for anything beyond a quick look, the `upstream-oracle` subagent (formerly `kaniko-oracle`) with the resolved checkout paths (`${KANIKO_SRC:-../kaniko}`, `../buildkit`), never reading upstream source in the main session or a fork |
| Changing `rnk-rootfs`, `rnk-sandbox`, `rnk-snapshot`, `rnk-store`, or set-up | plan mode first; `host-safety` subagent on the diff with `just review-pack <branch>` as its brief, then the fix loop below; then `red-team` |
| A claim about Linux or a runtime is unverified (`[inferred]`, "not checkable from the code") | `poc-runner` subagent: a minimal experiment, in the VM for anything privilege-related |
| Attack tests for a change, regression tests from upstream issues | `red-team` subagent: hostile inputs and T0–T2 tests, never product code |
| The maintainer decided; a diff needs its docs | `scribe` subagent: ADRs, answered questions, markers, cards, design, CHANGELOG, then `just docs-lint` |
| Performance numbers (W1–W3, benchmarks, Q-033, Q-245) | `bench-runner` subagent, only when no other VM runs |
| Final review | `reviewer` subagent: only the scope and the requirement, not your suspicions |
| Decision; before a PR | `/adr`; `/sync-docs [base]` (user-invoked) |
| Any diff; `rnk-rootfs`, `rnk-sandbox`, `rnk-layer`, `rnk-registry` | `/code-review`; `/security-review` |
| Rust review; flag design; property tests | Trail of Bits `rust-review`, `sharp-edges`, `property-based-testing` (user scope, if installed) |
| Milestones only | vendored `security-audit` (spawns many agents) |
| End of a phase, when the maintainer asks | `refactor` skill: behaviour-preserving clean-up, one crate per branch, digests held fixed |

`.claude/settings.json` denies `sudo`, `mount`, `unshare` and the like, and
asks before `cargo add`/`install`/`run`, `docker`, `podman`, `git commit`,
`git push`.

Models: `haiku` to locate code; `sonnet` to read, summarise, record or
measure (`upstream-oracle`, `scribe`, `poc-runner`, `bench-runner`); `opus`
for judgement (`host-safety`, `reviewer`, `red-team`). Continue a finished
agent with `SendMessage` rather than starting a new one.

### Merging parallel branches

- Forks never edit `docs/agents/phase-N.md`: its checklist lines are edited
  in place, so parallel branches conflict on it. The main session ticks it
  after each merge, and briefs say so.
- `.gitattributes` merges `docs/adr/README.md` and `CHANGELOG.md` with
  `merge=union`: branches only append rows or entries there, so both sides
  are kept instead of a conflict. Check the result's order after a merge.
  Files whose lines change in place (`phase-N.md`, `open-questions.md`,
  crate cards) keep normal merges: union would keep the old line next to
  the new one.
- Each fork gets its own range of question and ADR numbers in its brief.
- After merging, `just docs-lint` catches what the merge left behind.

### Host-safety fix loop

For a branch written by a fork (an agent in its own worktree):

1. The `host-safety` subagent reports to the main session.
2. The main session sends the findings back to the same fork (it keeps the
   task's context); it never starts a new agent for the fix.
3. The fork fixes on its branch, runs `just check` (and the VM suites when
   set-up or the sandbox changed) and reports.
4. The main session asks the same reviewer to re-check only the fix
   commits.
5. No blocker left: merge.

A test result counts only from the harness's exit status, never from an
agent reading a printed table: a suite that prints a verdict table must
fail on any unexpected cell (the `secrets` suite once printed `LEAK` and
exited 0).

High findings are fixed; cheap medium and low ones in the same round;
costly low ones, or ones that belong to a later task, become follow-ups in
the phase file and do not block. A finding that needs a design choice goes
to the maintainer as an open question while the fork builds the
fail-closed option as the interim. After three rounds still blocked, stop
and report to the maintainer.
