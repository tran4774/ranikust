---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0095: CI runs by lanes: a fast lane on drafts, the full run when ready, behind two gate checks

## Context and Problem Statement

On `main` on 2026-11-03, every push to a pull request ran all eleven jobs
of `.github/workflows/ci.yml` and `.github/workflows/t2.yml`
([ADR-0090](0090-t2-also-runs-as-root-on-github-hosted-runners.md)), even
a pull request that changed only Markdown, and the squash merge ran them
all again on `main`. The `main` ruleset required those eleven checks by
name ([ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)).
Every change of `Cargo.lock` made each caching job save a new rust-cache
entry while the old ones stayed, so the repository's Actions cache stood
at 10,296 MiB of its 10,240 MiB limit. The maintainer asked: "Tôi nghĩ
không phải MR nào cũng build hết" (I don't think every pull request needs
to build everything). The question: what runs on which change and event,
how the required checks stay meaningful when jobs are skipped, and how
the cache stays small.

## Considered Options

- Keep running every job on every push.
- Workflow-level `on: paths` or `paths-ignore` filters in `ci.yml` and
  `t2.yml`.
- A third-party change-detection action (such as `dorny/paths-filter` or
  `tj-actions/changed-files`).
- Change detection by a script in the repository, jobs skipped by `if:`,
  and one gate job per workflow as its only required check.

The first leaves the cost as it is. The second cannot work with required
checks: a workflow skipped as a whole by `paths` leaves its required checks
"Pending", so the pull request cannot merge [external]. The third adds a
third-party action with access to the job's token for a few lines of
logic; `tj-actions/changed-files` was compromised in March 2025 and leaked
the secrets of the workflows that ran it [external]. The fourth was
chosen, as part of the maintainer's answer below.

When the heavy jobs run, the maintainer was offered: on every push (as
before), when the pull request is marked ready for review, only when the
maintainer adds a label, or only from the "Run workflow" button.

## Decision Outcome

Accepted by the maintainer on 2026-11-03. To a proposal of six parts (A:
change detection inside the workflows; B: gate jobs as the only required
checks; C: cache sharing and pruning; D: remove the `check (aarch64 musl)`
job; E: lanes by draft state; F: timeouts and the release dry run's paths)
the maintainer answered "OK làm hết tất cả" (OK, do all of it); then asked
"ở PR có cách nào maintainer bấm cho chạy CI thì mới được chạy không?" (on
a pull request, is there a way for CI to run only when the maintainer
clicks?) and chose "Khi bạn mark Ready (Recommended)" (when you mark it
ready).

CI runs in lanes: a draft pull request runs a fast lane chosen by the paths
it changes; marking it ready for review, the `ci:full` label, a manual run
and every push to `main` run the full set; and the gates `CI result` and `T2
result` are the only required checks.

- **Change detection (A):** a `changes` job in each workflow runs
  `scripts/ci.py changes`, standard library and git only, no third-party
  action. On a pull request it reads `git diff --name-only --no-renames
  <base>...<head>` (a rename counts as both paths); a push to `main`, a
  manual run (`workflow_dispatch`) and any other event count as changing
  everything. It prints the outputs:
  - `rust`: `crates/**`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
    `.cargo/**`, the rustfmt and clippy configuration, `justfile` (the
    aarch64 job runs `just test-aarch64`) and `scripts/check-static.sh`;
  - `book`: `docs/book/**`;
  - `t2`: `rust`, or `tests/vm/**`, `tests/container/**`, `tests/smoke/**`;
  - `deny`: `Cargo.toml`, `Cargo.lock`, `crates/*/Cargo.toml`, `deny.toml`;
  - every lane: `.github/workflows/**`, `scripts/ci.py` and
    `.gitattributes` (it changes every checked-out file);
  - no lane: the rest of `docs/`, top-level Markdown, `.claude/`, the rest
    of `.github/`, `scripts/docs-lint.py` (its job runs every time),
    `scripts/review-pack.py`, `tests/ci/**` (the `changes` job runs those
    tests every time), what only the Release workflow reads
    (`scripts/release.py`, `scripts/third-party-licenses.hbs`,
    `tests/release/**`, `licenses/**`, `about.toml`, `LICENSE-*`,
    `NOTICE`, `cliff.toml`; the Release workflow has `paths` of its own)
    and what is local only (`tests/bench/**`, `tests/e2e/**`,
    `tests/registry/**`, `.gitignore`, `.editorconfig`);
  - a path no rule names runs every lane, and the script's test
    (`just test-ci`, also the first step of `ci.yml`'s `changes` job) fails
    on any tracked file no rule names, so a new path is classified when
    it is added;
  - `full`: true unless the event is a pull request that is a draft
    without the `ci:full` label;
  - `label_only`: a `labeled` or `unlabeled` event other than `ci:full` on
    a draft; then every lane is false and nothing runs.
  The event's values reach the script only through `env:`; the paths it
  logs are percent-encoded, so a file name cannot form a workflow command.
- **Lanes (E):** the fast lane runs on every pull request, drafts
  included: `rustfmt`, `clippy` and `test (x86_64 gnu)` when `rust`;
  `cargo-deny` when `deny`; `docs (rustdoc, mdBook)` when `rust` (rustdoc)
  or `book` (mdBook); a new `docs-lint` job (`scripts/docs-lint.py` with
  `DOCS_LINT_REQUIRE_GITHUB=1` and a token with `issues: read`) always.
  The full lane runs only when `full`: `static release build (x86_64
  musl)` and `test (aarch64 musl, native)` when `rust`, and the three T2
  jobs when `t2`.
- **Triggers:** `pull_request` with the types `opened`, `synchronize`,
  `reopened`, `ready_for_review`, `labeled` and `unlabeled`; `push` to
  `main`; `workflow_dispatch`. Never `on: paths` in `ci.yml` or `t2.yml`.
  A newer run of a pull request cancels the older one, as before, except a
  `label_only` run: its concurrency group is its own, so a label such as
  `agent`, added right after a pull request opens, cancels nothing.
- **Gates (B):** `CI result` (`ci.yml`) and `T2 result` (`t2.yml`) need
  every job of their workflow but the cache pruning, run `if: always()`,
  fail when the `changes` job did not succeed or any needed job failed or
  was cancelled, and pass when the rest succeeded or were skipped (a job
  skipped by `if:` reports success [external]). They become the `main`
  ruleset's only required checks, replacing the eleven job names. A gate
  carries its plain name only on a full run; on a draft's fast lane it is
  `CI result (draft, fast lane)` and `T2 result (draft, not run)`, and on
  a `label_only` run `... (label change, nothing run)`. GitHub judges a
  required check by the latest check run of that name on the commit
  [external], so a fast lane or a label change must not post a passing
  `CI result` over the commit's full run, or before it: after a draft is
  marked ready, the required checks wait for the full run.
- **Who clicks:** agents use the maintainer's GitHub account, so GitHub
  cannot tell who marked a pull request ready or added a label. The guard
  is the agents' ask rules: `gh pr ready` already asks
  ([ADR-0093](0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md)),
  and an agent asks the maintainer before marking a pull request ready
  (ADR-0087). An agent adds `ci:full` only when the maintainer asks, or to
  verify a pull request that changes CI itself; nothing but the rule holds
  that. "Run workflow" (`workflow_dispatch`) on the pull request's branch
  runs everything and posts its gates on the branch's head commit, which
  the pull request's required checks read.
- **Cache (C):** `T2 unit` and `T2c unit-c` build the same thing
  (`cargo-zigbuild test --no-run` for x86_64 musl, `-p` the crates holding
  T2 or T2c tests: the same six crates today, the same linker setting and
  environment), so they share the rust-cache key `t2-tests`. No other jobs
  share: they differ in target, profile or packages (the smoke job adds
  the `smoke-build` example and its dev-dependencies to the release build
  `static-build` makes). Caches are saved only from `main`, as before. On a
  push to `main`, a `caches` job in each workflow runs after the jobs that
  save caches and deletes each of their entries on `refs/heads/main` that
  a newer entry with the same restore key (the key without its files hash)
  supersedes (`scripts/ci.py stale-caches`, `gh cache list` and `gh cache
  delete` with the job's token). Only that job has `actions: write`; it
  never touches another ref's entries or another job's names.
- **aarch64 (D):** the `check (aarch64 musl)` job is removed: the native
  `test (aarch64 musl, native)` job builds and tests the same target. `just
  check-aarch64` stays as a quick local recipe, and `just ci` no longer
  runs it.
- **Limits (F):** every job of `ci.yml` and `t2.yml` has `timeout-minutes`
  (5 for the gates, 10 for `changes`, `rustfmt`, `cargo-deny`, `docs-lint`
  and the pruning, 20 for `clippy` and the docs, 30 for the tests, the
  static build and T2). The Release workflow's pull-request dry run no
  longer triggers on `README.md`, `NOTICE`, `LICENSE-APACHE` or
  `LICENSE-MIT`: the tarball only copies them, and a README edit would
  otherwise build a release from nothing; a tag packages them as they are.
- **Unchanged security terms:** `permissions: contents: read` by default
  and per-job grants only where needed (`docs-lint`: `issues: read`;
  `caches`: `actions: write`), `persist-credentials: false`, actions
  pinned by commit, no secrets, never `pull_request_target`.
- **Switch-over:** the maintainer replaces the ruleset's required checks
  with `CI result` and `T2 result` just before merging the pull request
  that brings this in. Until then the old ruleset expects `check (aarch64
  musl)`, which that pull request no longer has. Other open pull requests
  get the gates on their next push or merge of `main`; a re-run uses the
  workflow files and event of its first run.

### Consequences

- Good, because a draft that changes only documentation runs `changes`,
  `docs-lint` and the gates instead of eleven jobs, and a draft that
  changes Rust code gets its fast results without the native aarch64, the
  static build and the root jobs.
- Good, because the cache drops from 46 entries (10,296 MiB) to one entry
  per job after each `main` run, about 2 GB in steady state, below
  GitHub's limit with room for a `Cargo.lock` change.
- Good, because two required checks replace eleven names, so adding or
  renaming a job no longer needs a ruleset change.
- Bad, because a draft no longer gets the static build, the aarch64 tests
  or T2 unless someone asks (`ci:full`, "Run workflow"), so a problem
  only they show surfaces when the pull request is marked ready.
- Bad, because a path rule that misses what a job reads skips that job on
  a pull request; `main` still runs everything after the merge, so the
  miss shows there, after the fact. An unnamed path runs everything, which
  bounds this to a wrong rule, not a missing one.
- Bad, because the gates' names change with the kind of run, and a re-run
  of an old run repeats its old lanes (the event's draft state and labels
  of that time).
- Neutral, because each label change on a pull request still starts two
  short runs (`changes` and the gate, about half a minute each) that test
  nothing.

## Evidence

- Job times on `main` before this change (2026-11-03): rustfmt 23 s,
  clippy 34 s, test (x86_64 gnu) 123 s, static release build 97 s, check
  (aarch64 musl) 44 s, test (aarch64 musl, native) 259 s, cargo-deny 24 s,
  docs 41 s; T2 unit 150 to 180 s, T2c unit-c about 170 s, T2 smoke,
  quick, secrets 290 to 360 s [measured, GitHub Actions run history].
- The cache on 2026-11-03 (`gh cache list`): 46 entries, 10,296 MiB, of
  which 28 entries (6,304 MiB) are superseded entries of current jobs that
  the pruning deletes (`scripts/ci.py stale-caches` on that list), and 10
  (2,232 MiB) belong to the removed `check-aarch64` job and to the T2 keys
  `t2-unit` and `t2-unit-c` that `t2-tests` replaces, which GitHub evicts
  after seven days without access [external] or the maintainer deletes
  once. What stays: one entry per job and the two Zig tarballs, about
  2,041 MiB with the new `t2-tests` entry. The `t2-unit` and `t2-unit-c`
  entries of the same `Cargo.lock` were both 280 MiB, as two identical
  builds would be [measured].
- [code] `tests/vm/stage.sh` (`unit`) and `tests/container/lane.sh`
  (`unit-c`) run the same `cargo-zigbuild test --no-run --locked --target
  x86_64-unknown-linux-musl` with `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc`,
  each for the crates whose sources hold `#[ignore = "T2` (respectively
  `"T2c: `): `ranikust`, `rnk-context`, `rnk-rootfs`, `rnk-sandbox`,
  `rnk-snapshot` and `rnk-store` in both cases on 2026-11-03.
- [code] rust-cache v2.9.3 (`src/config.ts`) builds its key from
  `v0-rust`, the shared key or the key and the job id, the runner's OS and
  architecture, an environment hash (the restore key ends here) and a
  hash of the manifests and lock files.
- [external] GitHub: a job skipped by a condition reports success and
  satisfies a required check; a workflow skipped by `paths`, branch
  filters or a commit message leaves its checks pending; a job whose
  dependency failed is skipped, so a gate needs `always()`.
- The verification on this change's own pull request (the job matrix of
  its draft run, its `ci:full` run, and the Markdown-only case) is
  recorded on that pull request.

## More Information

Refines [ADR-0090](0090-t2-also-runs-as-root-on-github-hosted-runners.md):
T2 runs on full runs only, when the change needs it, and `unit` and
`unit-c` share their cache. Refines
[ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md):
the `main` ruleset requires the two gates instead of the job names, and
marking a pull request ready now also starts its full CI. Refines
[ADR-0009](0009-x86-64-first-aarch64-ready.md) in CI only: the native
aarch64 test job is the aarch64 check. Refines
[ADR-0089](0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md)'s
dry run, which no longer runs for the packaged documents alone.
Implemented in `scripts/ci.py`, `tests/ci/test_ci.py`,
`.github/workflows/ci.yml` and `.github/workflows/t2.yml`; described in
`docs/testing.md` (CI lanes) and `docs/agents/workflow.md` (GitHub flow).
Revisit if a path rule turns out to miss what a job reads, when agents get
a bot identity of their own (GitHub could then tell who marked a pull
request ready), or if GitHub adds workflow-level conditions that keep
required checks satisfied.
